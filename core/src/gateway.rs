// ---------------------------------------------------------------------------
// gateway.rs — the server's top-level request router.
//
// The app sends every request in one id-correlated envelope `{v,kind,id,...}`.
// This routes by `kind`: coconut → the federation handlers, `models` → the catalog.
// One entry point that the mixnet service-provider loop calls; replies keep the `id`
// for correlation. Chat and the paywall have their own handlers in the server crate.
//
// The model catalog here is a PLACEHOLDER (a couple of models with billing-derived
// retail rates) until the pricing table + provider integration are ported.
// ---------------------------------------------------------------------------

use serde_json::{json, Value};

use crate::billing::ceil_toku;
use crate::coconut::TOKU_PER_USD;
use crate::federation::{self, Authority};
use crate::quorum::QuorumStore;

/// Retail margin applied to provider cost for the displayed rate.
const MARGIN: f64 = 1.4;

/// Route one request. `request` is the raw envelope bytes; returns the reply bytes.
/// `quorum` records coconut serials (double-spend).
///
/// There is no session surface any more: a prompt pays with coins, so there is no balance
/// to hold, credit or read (docs/unlinkability.md, block D). `redeem` and `session.status`
/// went with it on 2026-09-15.
pub async fn handle(authority: &Authority, quorum: &mut QuorumStore, request: &[u8]) -> Vec<u8> {
    let v: Value = serde_json::from_slice(request).unwrap_or(Value::Null);
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    match v.get("kind").and_then(|k| k.as_str()).unwrap_or("") {
        "coconut" => federation::dispatch_enveloped(authority, quorum, request),
        "models" => encode(&models_reply(id)),
        other => encode(&json!({
            "id": id, "kind": "error", "error": format!("unknown kind: {other}")
        })),
    }
}

fn encode(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_default()
}

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
        let bytes =
            futures::executor::block_on(handle(auth, &mut quorum, &serde_json::to_vec(req).unwrap()));
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn models_kind_returns_a_catalog() {
        let auth = federation::bootstrap(1, 1, 32, 1702166400, crate::coconut::COIN_TOKU).unwrap();
        let reply = route(&auth[0], &json!({ "v": 1, "kind": "models", "id": "x" }));
        assert_eq!(reply["id"], "x");
        let models = reply["models"].as_array().unwrap();
        assert!(models.len() >= 1);
        assert!(models[0]["rate"]["in"].as_u64().unwrap() > 0);
        assert!(models[0]["model"].as_str().unwrap().len() > 0);
    }

    #[test]
    fn coconut_kind_routes_to_federation() {
        let auth = federation::bootstrap(1, 1, 32, 1702166400, crate::coconut::COIN_TOKU).unwrap();
        let reply = route(&auth[0], &json!({
            "v": 1, "kind": "coconut", "id": "y",
            "fed": serde_json::to_value(federation::FedRequest::Keys).unwrap(),
        }));
        assert_eq!(reply["id"], "y");
        assert!(reply.get("fed").is_some()); // coconut reply carries `fed`
    }

    #[test]
    fn unknown_kind_is_an_error_reply() {
        let auth = federation::bootstrap(1, 1, 32, 1702166400, crate::coconut::COIN_TOKU).unwrap();
        let reply = route(&auth[0], &json!({ "v": 1, "kind": "nonsense", "id": "z" }));
        assert_eq!(reply["kind"], "error");
    }

}
