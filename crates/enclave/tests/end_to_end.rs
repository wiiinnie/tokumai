//! The whole path, in one process: the app attests the (simulated) enclave, then signs and
//! seals requests to it. What is tested here is exactly what a real enclave will run — only
//! the attester, the key source and the model are stand-ins.

use serde_json::{json, Value};
use tokumai_attest::{sim, Policy};
use tokumai_core::account::{from_mnemonic, Account};
use tokumai_enclave::client::{attest_request, Session};
use tokumai_enclave::provider::MockProvider;
use tokumai_enclave::seal::FixedKeyProvider;
use tokumai_enclave::service::{Db, Enclave, Platform};

const ROOT: [u8; 32] = [5; 32];
const PHRASE: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon art";

fn enclave(dev: bool) -> Enclave {
    Enclave::start(Platform {
        attester: Box::new(sim::SimAttester::new(ROOT, "image-1")),
        keys: Box::new(FixedKeyProvider([9; 32])),
        provider: Box::new(MockProvider),
        db: Db::Memory,
        prices: [("mock".to_string(), (100, 400))].into_iter().collect(),
        dev_mode: dev,
    })
    .unwrap()
}

fn dev_policy() -> Policy {
    Policy { measurements: vec!["image-1".into()], simulated_root: Some(sim::root_public(&ROOT)), simulated_any_measurement: false }
}

async fn session(e: &Enclave, policy: &Policy) -> Result<Session, String> {
    let nonce: [u8; 32] = rand::random();
    let reply = e.handle(&attest_request(&nonce)).await;
    Session::from_attestation(&reply, &nonce, policy)
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

    let body = json!({ "model": "mock", "messages": [{ "role": "user", "content": "hello there" }], "max_tokens": 200 });
    let (pending, bytes) = s.request(&a, "chat", &body, tokumai_enclave::now_ms());
    let first = e.handle(&bytes).await;
    let answer = pending.open(&first).unwrap();
    assert!(answer["text"].as_str().unwrap().contains("hello there"));
    let cost = answer["cost"].as_u64().unwrap();
    assert!(cost > 0 && cost < 100, "charged what the answer cost, not the ceiling: {cost}");
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
    let r = call(&e, &s, &a, "chat", json!({ "model": "mock", "messages": [{ "role": "user", "content": "hi" }] })).await;
    assert!(r["error"].as_str().unwrap().contains("not enough credit"));
}
