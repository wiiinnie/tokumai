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
            witness: None,
            admin: None,
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

    // The same bytes once more, after the kept answer is gone: refused in the clear. Never
    // a second message sealed under that request's key (audit H1, 2026-10-05).
    e.forget_kept_replies();
    let later: Value = serde_json::from_slice(&e.handle(&bytes).await).unwrap();
    assert_eq!(later["kind"], "error");
    assert!(later["error"].as_str().unwrap().contains("already made"));
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
    // A decline is an answer in the chat's own shape — no prose, a reason, nothing charged —
    // not an error (since "Each provider filters its own questions", 2026-09).
    for _ in 0..3 {
        let r = call(&e, &s, &a, "chat", ask.clone()).await;
        assert_eq!(r["declined"], true, "{r}");
        assert!(r["why"].as_str().unwrap().starts_with("Declined by"), "{r}");
        assert_eq!(r["cost"], 0, "{r}");
    }
    let r = call(&e, &s, &a, "chat", ask).await;
    assert_eq!(r["declined"], true, "{r}");
    assert!(r["why"].as_str().unwrap().contains("declined by"), "{r}");
    assert!(r["strikes"]["untilMs"].as_u64().is_some(), "the pause says until when: {r}");
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

#[tokio::test]
async fn a_small_balance_shortens_the_answer_and_an_empty_one_says_so_plainly() {
    let e = enclave(true);
    let s = session(&e, &dev_policy()).await.unwrap();
    let a = from_mnemonic(PHRASE).unwrap();
    let ask = json!({ "model": "mock", "messages": [{ "role": "user", "content": "hello" }], "maxTokens": 4096 });
    let empty = call(&e, &s, &a, "chat", ask.clone()).await;
    assert_eq!(empty["noCredit"], true);
    assert!(empty["error"].as_str().unwrap().contains("balance is 0 TOKU"), "{empty}");
    // Far less than the worst case of 4096 answer tokens, enough for a short answer.
    call(&e, &s, &a, "dev.credit", json!({ "toku": 2_000 })).await;
    let answer = call(&e, &s, &a, "chat", ask.clone()).await;
    assert_eq!(answer["kind"], "chat", "{answer}");
    // The mock's answer is short: it did not run into the lowered limit, so no note.
    assert_eq!(answer["capped"], false);
    assert!(answer["cost"].as_u64().unwrap() <= 2_000);
    // Too little for even a short answer: refused, with the numbers.
    let e2 = enclave(true);
    let s2 = session(&e2, &dev_policy()).await.unwrap();
    call(&e2, &s2, &a, "dev.credit", json!({ "toku": 3 })).await;
    let tiny = call(&e2, &s2, &a, "chat", ask).await;
    assert_eq!(tiny["noCredit"], true, "{tiny}");
}

/// The Memory Palace end to end: two devices of one phrase write and read sealed blobs
/// under throwaway envelope keys, concurrent writes conflict and merge, and a question
/// with the palace's index makes the model ask for a file, which the app answers.
#[tokio::test]
async fn the_memory_palace_syncs_two_devices_and_the_model_reads_one_file() {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    use tokumai_core::palace::{self as palace, MemFile, PalaceKeys, SignedFields};
    let e = enclave(true);
    let s = session(&e, &dev_policy()).await.unwrap();
    let keys = PalaceKeys::from_mnemonic(PHRASE).unwrap();
    // What the app sends: a palace body signed by the palace key, in an envelope signed by
    // an account made for that one request.
    let palace_call = |op: &'static str, blob: String, blobs: Vec<String>, expected: u64, ct: Vec<u8>| {
        let nonce = palace::new_id("nonce_") + &palace::new_id("");
        let ts = tokumai_enclave::now_ms();
        let joined = blobs.join(",");
        let (b, v) = match op {
            "get" => (joined.clone(), 0),
            "put" => (blob.clone(), expected + 1),
            "delete" => (blob.clone(), expected),
            _ => (String::new(), 0),
        };
        let hash = palace::ct_hash(&ct);
        let sig = keys.sign_request(&SignedFields { op, blob_id: &b, version: v, ct_sha256: &hash, ts, nonce: &nonce });
        (format!("palace.{op}"), json!({ "ns": keys.namespace(), "pub": keys.sign_pub_hex(), "ts": ts, "nonce": nonce, "sig": sig,
            "blobId": blob, "blobIds": blobs, "expected": expected, "ct": B64.encode(&ct) }))
    };
    let throwaway = || tokumai_core::account::create_account();
    let file_blob = keys.blob_id("f_work");
    let line = |id: &str, text: &str, t: u64| palace::Line { id: id.into(), text: text.into(), never_send: false, updated: t, deleted: false, h: String::new() };

    // Device A writes the first version.
    let a_file = MemFile { lines: vec![line("l_1", "Works at [EMPLOYER_1]", 1)], ..MemFile::new("f_work") };
    let (op, body) = palace_call("put", file_blob.clone(), vec![], 0, keys.seal(&file_blob, 1, &serde_json::to_vec(&a_file).unwrap()));
    assert_eq!(call(&e, &s, &throwaway(), &op, body).await["version"], 1);
    // Device B, same phrase, never synced: its write is stale and conflicts…
    let b_file = MemFile { lines: vec![line("l_2", "Leads a team of 12", 2)], ..MemFile::new("f_work") };
    let (op, body) = palace_call("put", file_blob.clone(), vec![], 0, keys.seal(&file_blob, 1, &serde_json::to_vec(&b_file).unwrap()));
    let conflict = call(&e, &s, &throwaway(), &op, body).await;
    assert_eq!((conflict["kind"].as_str(), conflict["version"].as_u64()), (Some("palace.conflict"), Some(1)));
    // …so it fetches, merges, and writes again.
    let (op, body) = palace_call("get", String::new(), vec![file_blob.clone()], 0, vec![]);
    let got = call(&e, &s, &throwaway(), &op, body).await;
    let ct = B64.decode(got["blobs"][0]["ct"].as_str().unwrap()).unwrap();
    let theirs: MemFile = serde_json::from_slice(&keys.open(&file_blob, 1, &ct).unwrap()).unwrap();
    let merged = palace::merge_file(&b_file, &theirs);
    let (op, body) = palace_call("put", file_blob.clone(), vec![], 1, keys.seal(&file_blob, 2, &serde_json::to_vec(&merged).unwrap()));
    assert_eq!(call(&e, &s, &throwaway(), &op, body).await["version"], 2);
    // Device A lists, sees version 2, and gets both lines.
    let (op, body) = palace_call("list", String::new(), vec![], 0, vec![]);
    let list = call(&e, &s, &throwaway(), &op, body).await;
    assert_eq!(list["blobs"][0]["version"], 2);
    let (op, body) = palace_call("get", String::new(), vec![file_blob.clone()], 0, vec![]);
    let got = call(&e, &s, &throwaway(), &op, body).await;
    let ct = B64.decode(got["blobs"][0]["ct"].as_str().unwrap()).unwrap();
    let now: MemFile = serde_json::from_slice(&keys.open(&file_blob, 2, &ct).unwrap()).unwrap();
    assert_eq!(now.lines.len(), 2, "no line lost");

    // The account asks a question with the palace's index. The model (the mock) asks for
    // the file; the app answers from its own copy; the model answers with it. Each round
    // is billed; nothing of it is kept.
    let a = from_mnemonic(PHRASE).unwrap();
    call(&e, &s, &a, "dev.credit", json!({ "toku": 100_000 })).await;
    let memory = json!({ "index": [{ "id": "f_work", "title": "Work", "description": "Job, role, team size" }] });
    let mut messages = vec![json!({ "role": "user", "content": "What do you remember about my work?" })];
    let first = call(&e, &s, &a, "chat", json!({ "model": "mock", "messages": messages, "memory": memory })).await;
    let calls = first["toolCalls"].as_array().expect("the model asks for a file").clone();
    assert_eq!(calls[0]["args"]["file_id"], "f_work");
    assert!(first["cost"].as_u64().unwrap() > 0);
    messages.push(json!({ "role": "assistant", "content": "", "toolCalls": calls }));
    messages.push(json!({ "role": "tool", "toolCallId": calls[0]["id"], "name": "read_memory", "content": "- Works at [EMPLOYER_1]\n- Leads a team of 12" }));
    let answer = call(&e, &s, &a, "chat", json!({ "model": "mock", "messages": messages, "memory": memory })).await;
    assert!(answer["toolCalls"].is_null());
    assert!(answer["text"].as_str().unwrap().contains("Leads a team of 12"), "{answer}");
    // A result larger than the enclave takes is refused before any model is asked.
    messages.pop();
    messages.push(json!({ "role": "tool", "toolCallId": calls[0]["id"], "name": "read_memory", "content": "x".repeat(20_000) }));
    assert_eq!(call(&e, &s, &a, "chat", json!({ "model": "mock", "messages": messages, "memory": memory })).await["kind"], "error");
}
