//! The whole path, in one process: the app attests the (simulated) enclave, then signs and
//! seals requests to it. What is tested here is exactly what a real enclave will run — only
//! the attester, the key source and the model are stand-ins.

use serde_json::{json, Value};
use tokumai_attest::{sim, Policy};
use tokumai_core::account::{from_mnemonic, Account};
use tokumai_proto::session::{attest_request, Session};
use tokumai_core::pricing::PricingTable;
use tokumai_enclave::policy::PRICING_JSON;
use tokumai_enclave::provider::{BoxFuture, Call, Completion, Provider, Providers};
use tokumai_enclave::seal::FixedKeyProvider;
use tokumai_enclave::service::{Db, Enclave, Platform};

const ROOT: [u8; 32] = [5; 32];
const PHRASE: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon art";

/// A stand-in for a provider that declines everything, the way OpenAI or Google do.
struct Refuser;
impl Provider for Refuser {
    fn name(&self) -> &'static str {
        "refuser"
    }
    fn serves(&self, model: &str) -> bool {
        model == "gemini-3.5-flash-lite"
    }
    fn complete<'a>(&'a self, _call: &'a Call<'a>) -> BoxFuture<'a, Result<Completion, String>> {
        Box::pin(async { Err("Declined by Google (SAFETY): no".to_string()) })
    }
}

fn with_providers(dev: bool, providers: Providers) -> Enclave {
    Enclave::start(Platform {
        attester: Box::new(sim::SimAttester::new(ROOT, "image-1")),
        keys: Box::new(FixedKeyProvider([9; 32])),
        providers,
        db: Db::Memory,
        pricing: PricingTable::parse(PRICING_JSON).unwrap(),
        dev_mode: dev,
        stripe: None,
        apple_api: None,
    })
    .unwrap()
}

fn enclave(dev: bool) -> Enclave {
    with_providers(dev, Providers::mock())
}

fn dev_policy() -> Policy {
    Policy { measurements: vec!["image-1".into()], simulated_root: Some(sim::root_public(&ROOT)), simulated_any_measurement: false }
}

async fn session(e: &Enclave, policy: &Policy) -> Result<Session, String> {
    let nonce: [u8; 32] = rand::random();
    let reply = e.handle(&attest_request(&nonce)).await;
    Session::from_attestation(&reply, &nonce, policy, None)
}

async fn call(e: &Enclave, s: &Session, a: &Account, op: &str, body: Value) -> Value {
    let (pending, bytes) = s.request(a, op, &body, tokumai_enclave::now_ms());
    pending.open(&e.handle(&bytes).await).unwrap()
}

#[tokio::test]
async fn an_account_pays_for_a_question_exactly_once_and_only_what_it_cost() {
    let e = enclave(true);
    let s = session(&e, &dev_policy()).await.unwrap();
    let a = from_mnemonic(PHRASE).unwrap();

    let credited = call(&e, &s, &a, "dev.credit", json!({ "toku": 100_000 })).await;
    assert_eq!(credited["balance"]["total"], 100_000);

    let body = json!({ "model": "mock", "messages": [{ "role": "user", "content": "hello there" }], "maxTokens": 200 });
    let (pending, bytes) = s.request(&a, "chat", &body, tokumai_enclave::now_ms());
    let first = e.handle(&bytes).await;
    let answer = pending.open(&first).unwrap();
    assert!(answer["text"].as_str().unwrap().contains("hello there"));
    let cost = answer["cost"].as_u64().unwrap();
    assert!(cost > 0 && cost < 20, "charged what the answer cost, not the ceiling: {cost}");
    assert_eq!(answer["balance"].as_u64().unwrap(), 100_000 - cost);

    // The answer was lost; the app sends the same bytes again: same answer, no second charge.
    let again = e.handle(&bytes).await;
    assert_eq!(again, first);
    let balance = call(&e, &s, &a, "balance", json!({})).await;
    assert_eq!(balance["balance"]["total"].as_u64().unwrap(), 100_000 - cost);
}

#[tokio::test]
async fn a_release_build_refuses_the_simulated_enclave() {
    let e = enclave(true);
    let release = Policy { measurements: vec!["image-1".into()], ..Default::default() };
    let refused = session(&e, &release).await.err().expect("refused");
    assert!(refused.contains("not accepted"), "{refused}");
}

#[tokio::test]
async fn a_request_for_another_account_or_another_enclave_is_refused() {
    let e = enclave(true);
    let other = enclave(true);
    let s = session(&e, &dev_policy()).await.unwrap();
    let a = from_mnemonic(PHRASE).unwrap();
    // Sealed to e, sent to another enclave: it cannot even be opened there.
    let (_, bytes) = s.request(&a, "balance", &json!({}), tokumai_enclave::now_ms());
    let reply: Value = serde_json::from_slice(&other.handle(&bytes).await).unwrap();
    assert!(reply["error"].as_str().unwrap().contains("attest again"));
    // A request whose clock is an hour off is refused.
    let (p, bytes) = s.request(&a, "balance", &json!({}), tokumai_enclave::now_ms() - 3_600_000);
    assert!(p.open(&e.handle(&bytes).await).unwrap()["error"].as_str().unwrap().contains("clock"));
}

#[tokio::test]
async fn nothing_is_credited_out_of_thin_air_outside_development() {
    let e = enclave(false);
    let s = session(&e, &dev_policy()).await.unwrap();
    let a = from_mnemonic(PHRASE).unwrap();
    let r = call(&e, &s, &a, "dev.credit", json!({ "toku": 1 })).await;
    assert_eq!(r["error"], "not available on this enclave");
    // The mock has no price of its own in pricing.json, and outside development a model
    // without a published price is not offered at all (fail closed).
    let r = call(&e, &s, &a, "chat", json!({ "model": "mock", "messages": [{ "role": "user", "content": "hi" }] })).await;
    assert_eq!(r["error"], "that model is not offered");
}

#[tokio::test]
async fn attachments_the_providers_cannot_read_are_refused_before_anything_is_sent() {
    let e = enclave(true);
    let s = session(&e, &dev_policy()).await.unwrap();
    let a = from_mnemonic(PHRASE).unwrap();
    call(&e, &s, &a, "dev.credit", json!({ "toku": 100_000 })).await;
    let r = call(&e, &s, &a, "chat", json!({ "model": "mock", "messages": [{ "role": "user", "content": "listen",
        "attachments": [{ "mimeType": "audio/ogg", "data": "AAAA" }] }] })).await;
    assert!(r["error"].as_str().unwrap().contains("not supported"), "{r}");
    assert_eq!(call(&e, &s, &a, "balance", json!({})).await["balance"]["total"], 100_000, "and nothing was held");
}

#[tokio::test]
async fn declines_cost_nothing_and_three_a_day_pause_that_provider() {
    let e = with_providers(true, Providers::with(vec![Box::new(Refuser)]));
    let s = session(&e, &dev_policy()).await.unwrap();
    let a = from_mnemonic(PHRASE).unwrap();
    call(&e, &s, &a, "dev.credit", json!({ "toku": 100_000 })).await;
    let ask = json!({ "model": "gemini-3.5-flash-lite", "messages": [{ "role": "user", "content": "x" }] });
    for _ in 0..3 {
        let r = call(&e, &s, &a, "chat", ask.clone()).await;
        assert!(r["error"].as_str().unwrap().starts_with("Declined by"), "{r}");
    }
    let r = call(&e, &s, &a, "chat", ask).await;
    assert!(r["error"].as_str().unwrap().contains("tomorrow"), "{r}");
    assert_eq!(call(&e, &s, &a, "balance", json!({})).await["balance"]["total"], 100_000, "no decline was charged");
}

#[tokio::test]
async fn the_plan_ladder_is_offered_and_card_plans_need_stripe() {
    let e = enclave(false);
    let s = session(&e, &dev_policy()).await.unwrap();
    let a = from_mnemonic(PHRASE).unwrap();
    let ladder = call(&e, &s, &a, "plans", json!({})).await;
    assert_eq!(ladder["kind"], "plans");
    assert_eq!(ladder["tiers"].as_array().unwrap().len(), 3);
    assert_eq!(ladder["byCard"], false);
    assert!(ladder["plan"].is_null());
    let order = call(&e, &s, &a, "plan.create", json!({ "tier": 0 })).await;
    assert!(order["error"].as_str().unwrap().contains("not sold by card"));
    let change = call(&e, &s, &a, "plan.change", json!({ "tier": 1 })).await;
    assert!(change["error"].is_string());
    // No plan yet: the balance says so; the renewal check has nothing to ask about.
    assert!(call(&e, &s, &a, "balance", json!({})).await["plan"].is_null());
    e.tick().await;
}

#[tokio::test]
async fn an_app_store_transaction_that_does_not_verify_is_final_and_credits_nothing() {
    let e = enclave(false);
    let s = session(&e, &dev_policy()).await.unwrap();
    let a = from_mnemonic(PHRASE).unwrap();
    let reply = call(&e, &s, &a, "iap.verify", json!({ "jws": "not.a.jws" })).await;
    assert!(reply["error"].is_string());
    assert_eq!(reply["final"], true, "the app finishes it instead of re-sending forever");
    assert_eq!(call(&e, &s, &a, "balance", json!({})).await["balance"]["total"], 0);
}

#[tokio::test]
async fn the_proof_names_the_address_the_enclave_listens_at() {
    let e = enclave(true);
    e.set_address("enclave.nym");
    let nonce: [u8; 32] = rand::random();
    let reply = e.handle(&attest_request(&nonce)).await;
    let s = Session::from_attestation(&reply, &nonce, &dev_policy(), Some("enclave.nym")).unwrap();
    assert_eq!(s.address, "enclave.nym");
    // Reached through some other address: whatever forwards there is not the enclave's client.
    let relayed = Session::from_attestation(&reply, &nonce, &dev_policy(), Some("relay.nym")).err().expect("refused");
    assert!(relayed.contains("in between"), "{relayed}");
    // An answer whose address was swapped does not match its proof.
    let mut v: Value = serde_json::from_slice(&reply).unwrap();
    v["address"] = json!("relay.nym");
    let forged = Session::from_attestation(&serde_json::to_vec(&v).unwrap(), &nonce, &dev_policy(), Some("relay.nym")).err().expect("refused");
    assert!(forged.contains("other keys"), "{forged}");
}
