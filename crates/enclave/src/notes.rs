//! The enclave's end of the blind notes: the month keys, minting, redeeming.
//!
//! A payment no longer credits an account. It buys a note — one paid month of one tier —
//! that the enclave signs blind (`tokumai_core::notes`) and an account redeems later.
//! What the book records of a minting is the fingerprint of the blinded message, under a
//! keyed hash of the payment's reference and month; what it records of a redemption is
//! the note's nonce as spent. No row joins the two: the enclave signed something it could
//! not read, and what it later verifies it had never seen. Design: docs/blind-tokens.md.
//!
//! The keys are derived from the data key, one per calendar month, so every start makes
//! the same keys and nothing is stored; their public halves travel in every attestation,
//! bound by the proof, so that an enclave cannot hand one person keys of its own.

use crate::apple;
use crate::plans;
use crate::service::{error, Enclave};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokumai_core::notes::{self as core_notes, KeyPair, Note, GRACE_MS};
use tokumai_core::subscription::TIERS;

/// The month keys, made on demand from the data key and kept for the run.
pub struct Mint {
    data_key: [u8; 32],
    keys: Mutex<HashMap<u16, Arc<KeyPair>>>,
}

impl Mint {
    pub fn new(data_key: [u8; 32]) -> Mint {
        Mint { data_key, keys: Mutex::new(HashMap::new()) }
    }

    fn key(&self, epoch: u16) -> Result<Arc<KeyPair>, String> {
        if let Some(k) = self.keys.lock().ok().and_then(|m| m.get(&epoch).cloned()) {
            return Ok(k);
        }
        // Made outside the lock: a 2048-bit key takes a moment, and attestations must not
        // queue behind it.
        let made = Arc::new(core_notes::keypair_for(&self.data_key, epoch)?);
        let mut m = self.keys.lock().map_err(|_| "the month keys are unavailable".to_string())?;
        Ok(m.entry(epoch).or_insert(made).clone())
    }

    /// The months open at `now` (`tokumai_core::notes::window`).
    pub fn window(now_ms: u64) -> (u16, u16) {
        core_notes::window(now_ms)
    }

    /// The public keys of the window, for the attestation.
    pub fn published(&self, now_ms: u64) -> Result<Vec<(u16, Vec<u8>)>, String> {
        let (from, to) = Mint::window(now_ms);
        (from..=to).map(|e| Ok((e, core_notes::public_to_spki(&self.key(e)?.pk)?))).collect()
    }

    /// Make this month's keys before the first attestation asks for them.
    pub fn warm(&self, now_ms: u64) {
        let _ = self.published(now_ms);
    }
}

/// What a payment proves: the plan's tier, whether it is a year, the paid period, and the
/// rail reference the minting is deduplicated by.
struct Proof {
    tier: u8,
    yearly: bool,
    start_ms: u64,
    until_ms: u64,
    rail: String,
}

fn epochs_of(p: &Proof) -> Vec<u16> {
    core_notes::epochs_covered(p.start_ms, p.until_ms, p.yearly)
}

fn body_of(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or(Value::Null)
}

fn b64(v: &Value, key: &str) -> Option<Vec<u8>> {
    v.get(key).and_then(|x| x.as_str()).and_then(|x| B64.decode(x).ok())
}

impl Enclave {
    async fn proof_of(&self, proof: &Value, now: u64) -> Result<Proof, String> {
        match proof.get("kind").and_then(|k| k.as_str()).unwrap_or("") {
            "apple" => {
                let jws = proof.get("jws").and_then(|j| j.as_str()).unwrap_or("");
                let tx = apple::verify_jws(jws, now)?;
                if apple::revoked_plan(&tx) {
                    return Err("this plan was refunded by Apple and has ended".into());
                }
                let (tier, yearly) = apple::plan_for(&tx, now)?;
                Ok(Proof { tier: tier as u8, yearly, start_ms: tx.purchased_at_ms, until_ms: tx.expires_at_ms, rail: format!("iap:{}", tx.original_transaction_id) })
            }
            "stripe" => {
                let Some(stripe) = &self.stripe else { return Err("plans are not sold by card here".into()) };
                let sub = proof.get("subscription").and_then(|s| s.as_str()).unwrap_or("").trim().to_string();
                if sub.is_empty() {
                    return Err("no subscription named".into());
                }
                let st = stripe.subscription(&sub).await?;
                if !st.paid {
                    return Err("this subscription is not paid".into());
                }
                if st.back != crate::stripe::MoneyBack::None {
                    return Err("this subscription was refunded and has ended".into());
                }
                Ok(Proof { tier: st.tier as u8, yearly: st.yearly, start_ms: st.start_ms, until_ms: st.end_ms, rail: format!("stripe:{sub}") })
            }
            _ => Err("a proof is an App Store transaction or a Stripe subscription".into()),
        }
    }

    /// `note.mint`: a proof of payment and a blinded note in, a blind signature out. Not
    /// signed by any account that matters — the app sends it under a key it throws away,
    /// and the enclave records the payment's month as minted, nothing about who asked.
    ///
    /// The same payment and month again: signed again, the same blinded message, the same
    /// signature — that is how a phone restored from its phrase gets its note back. A
    /// DIFFERENT blinded message for a month already minted is refused, which is what
    /// stops one payment from becoming two notes.
    pub(crate) async fn note_mint(&self, body: &str, now: u64) -> Value {
        let b = body_of(body);
        let Some(blinded) = b64(&b, "blinded") else { return error("no blinded note") };
        let Some(epoch) = b.get("epoch").and_then(|e| e.as_u64()).and_then(|e| u16::try_from(e).ok()) else { return error("no month named") };
        let proof = match self.proof_of(b.get("proof").unwrap_or(&Value::Null), now).await {
            Ok(p) => p,
            Err(e) => return json!({ "kind": "error", "error": e, "final": true }),
        };
        if !epochs_of(&proof).contains(&epoch) {
            return json!({ "kind": "error", "error": "this payment does not cover that month", "final": true });
        }
        let (from, to) = Mint::window(now);
        if epoch < from || epoch > to {
            return error("that month is not open for minting yet, or not any more");
        }
        let reference = format!("note:mint:{}:{epoch}", proof.rail);
        let fingerprint = core_notes::blinded_fingerprint(&blinded);
        {
            let Ok(l) = self.ledger.lock() else { return error("ledger unavailable") };
            match l.minted_get(&reference) {
                Ok(Some(seen)) if seen != fingerprint => {
                    return json!({ "kind": "error", "error": "a note for this month was already minted for this payment", "final": true })
                }
                Ok(Some(_)) => {}
                Ok(None) => {
                    if let Err(e) = l.minted_put(&reference, &fingerprint) {
                        return error(&e);
                    }
                }
                Err(e) => return error(&e),
            }
        }
        let key = match self.notes.key(epoch) {
            Ok(k) => k,
            Err(e) => return error(&e),
        };
        match core_notes::blind_sign(&key.sk, &blinded) {
            Ok(sig) => json!({ "kind": "note.minted", "epoch": epoch, "tier": proof.tier, "sig": B64.encode(sig) }),
            Err(e) => error(&e),
        }
    }

    /// `note.redeem`: plain notes in, this month's allowance out. Signed by the account the
    /// allowance goes to. Each note is checked under its month's key, refused if that
    /// month is not open, and spent exactly once; the account's plan row then carries
    /// `note:<its own name>` as its rail — a rail that leads nowhere, by design.
    pub(crate) fn note_redeem(&self, account: &str, body: &str, now: u64) -> Value {
        let b = body_of(body);
        let Some(list) = b.get("notes").and_then(|n| n.as_array()) else { return error("no notes") };
        if list.is_empty() || list.len() > 12 {
            return error("one to twelve notes at a time");
        }
        let (from, to) = Mint::window(now);
        let mut parsed = Vec::with_capacity(list.len());
        for item in list {
            let (Some(raw), Some(sig)) = (b64(item, "note"), b64(item, "sig")) else { return error("a note is its bytes and its signature") };
            let note = match Note::parse(&raw) {
                Ok(n) => n,
                Err(e) => return json!({ "kind": "error", "error": e, "final": true }),
            };
            if note.epoch < from || note.epoch > to {
                return json!({ "kind": "error", "error": format!("the note for month {} is not redeemable now", note.epoch), "final": true });
            }
            if note.tier as usize >= TIERS.len() {
                return json!({ "kind": "error", "error": "the note names no plan", "final": true });
            }
            let key = match self.notes.key(note.epoch) {
                Ok(k) => k,
                Err(e) => return error(&e),
            };
            if !core_notes::verify(&key.pk, &note, &sig) {
                return json!({ "kind": "error", "error": "the note's signature does not check out", "final": true });
            }
            parsed.push(note);
        }
        let mut granted = 0usize;
        let mut again = 0usize;
        {
            let Ok(l) = self.ledger.lock() else { return error("ledger unavailable") };
            let key = l.acct_key(account);
            let rail = format!("note:{key}");
            for note in &parsed {
                let spent = format!("note:spent:{}:{}", note.epoch, hex::encode(note.nonce));
                match l.first_payment(&spent, now) {
                    Ok(true) => {}
                    Ok(false) => {
                        again += 1;
                        continue;
                    }
                    Err(e) => return error(&e),
                }
                // The allowance runs from now until the note's month is over, plus the
                // grace in which the next one may arrive late. Set, never added: a note
                // redeemed while the old month still has leftover replaces it, which is
                // what "it does not roll over" means.
                let until = core_notes::epoch_end_ms(note.epoch) + GRACE_MS;
                if let Err(e) = plans::subscribe_or_renew(&l, account, note.tier as usize, false, &rail, now, now, until) {
                    return error(&e);
                }
                granted += 1;
            }
            if granted > 0 {
                // From here the account's next question is the one that could be tied to
                // the payment — `cover` watches for it. With the guard held, as it must be.
                self.cover.paid(&key, now);
            }
        }
        json!({ "kind": "note.redeemed", "granted": granted, "again": again, "plan": self.plan_summary(account) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{Db, Platform};
    use tokumai_core::notes as core_notes;

    fn enclave() -> &'static Enclave {
        let e = Enclave::start(Platform {
            attester: Box::new(tokumai_attest::sim::SimAttester::new([5; 32], "image-1")),
            keys: Box::new(crate::seal::FixedKeyProvider([9; 32])),
            providers: crate::provider::Providers::mock(),
            db: Db::Memory,
            pricing: tokumai_core::pricing::PricingTable::parse(crate::policy::PRICING_JSON).unwrap(),
            dev_mode: false,
            stripe: None,
            apple_api: None,
            witness: None,
        })
        .unwrap();
        Box::leak(Box::new(e))
    }

    fn proof(start_ms: u64, months: u32, yearly: bool) -> Proof {
        Proof { tier: 1, yearly, start_ms, until_ms: tokumai_core::subscription::add_months_ms(start_ms, months), rail: "iap:2000000111".into() }
    }

    #[test]
    fn a_period_mints_the_month_it_ends_in_and_a_year_mints_twelve() {
        let sep28 = tokumai_core::subscription::ms_from_civil(2026, 9, 28) + 3_600_000;
        assert_eq!(epochs_of(&proof(sep28, 1, false)), vec![9]); // ends Oct 28 → October
        let year = epochs_of(&proof(sep28, 12, true));
        assert_eq!(year.len(), 12);
        assert_eq!(year[0], 9);
        assert_eq!(year[11], 20); // ends Sep 28, 2027 → September 2027
        // A period ending exactly on the 1st belongs to the month before.
        let oct1 = tokumai_core::subscription::ms_from_civil(2026, 10, 1);
        assert_eq!(epochs_of(&Proof { tier: 0, yearly: false, start_ms: tokumai_core::subscription::ms_from_civil(2026, 9, 1), until_ms: oct1, rail: "x".into() }), vec![8]);
    }

    /// The whole round against a real enclave in memory: the app blinds with the key the
    /// enclave publishes, the enclave mints against a (test) proof, the app unblinds, the
    /// account redeems and holds the month's allowance; the same note again adds nothing;
    /// a second blinded message for the same payment and month is refused; a restored
    /// phone's identical blinded message is signed again, identically.
    #[tokio::test]
    async fn a_note_goes_from_payment_to_allowance_once() {
        let e = enclave();
        let now = tokumai_core::subscription::ms_from_civil(2026, 10, 5);
        let cur = core_notes::epoch_of_ms(now);
        let keys = e.notes.published(now).unwrap();
        assert_eq!(keys.iter().map(|(k, _)| *k).collect::<Vec<_>>(), vec![cur - 1, cur, cur + 1]);
        let pk = core_notes::public_from_spki(&keys[1].1).unwrap();
        let seed = [4u8; 32];
        let note = core_notes::note_for(&seed, 1, cur);
        let (blinded, secret) = core_notes::blind(&pk, &note, &seed).unwrap();

        // Minting, through the same path the operation takes, with the proof already
        // verified: the dedup, the window, the signature.
        let p = Proof { tier: 1, yearly: false, start_ms: now - 10 * 86_400_000, until_ms: now + 20 * 86_400_000, rail: "iap:2000000111".into() };
        let reference = format!("note:mint:{}:{cur}", p.rail);
        let fp = core_notes::blinded_fingerprint(&blinded);
        {
            let l = e.ledger.lock().unwrap();
            assert_eq!(l.minted_get(&reference).unwrap(), None);
            l.minted_put(&reference, &fp).unwrap();
            assert_eq!(l.minted_get(&reference).unwrap(), Some(fp.clone()));
        }
        let sig_b = core_notes::blind_sign(&e.notes.key(cur).unwrap().sk, &blinded).unwrap();
        let sig = core_notes::finalize(&pk, &sig_b, &secret, &note).unwrap();

        // Redeeming, through the operation.
        let body = json!({ "notes": [{ "note": B64.encode(note.to_bytes()), "sig": B64.encode(&sig) }] }).to_string();
        let r = e.note_redeem("acct-a", &body, now);
        assert_eq!(r["kind"], "note.redeemed", "{r}");
        assert_eq!(r["granted"], 1);
        assert_eq!(r["plan"]["rail"], "note");
        assert_eq!(r["plan"]["granted"], TIERS[1].0);
        assert_eq!(r["plan"]["periodEnd"], core_notes::epoch_end_ms(cur) + GRACE_MS);
        // Again: spent, nothing added, and said so.
        let r2 = e.note_redeem("acct-a", &body, now + 1000);
        assert_eq!((r2["granted"].as_u64(), r2["again"].as_u64()), (Some(0), Some(1)), "{r2}");
        // Another account with the same note: spent for them too.
        let r3 = e.note_redeem("acct-b", &body, now + 2000);
        assert_eq!(r3["granted"], 0);
        assert_eq!(r3["plan"], Value::Null);
        // A note under the wrong month's key, or bent, is refused before anything is written.
        let wrong = json!({ "notes": [{ "note": B64.encode(core_notes::note_for(&seed, 1, cur + 1).to_bytes()), "sig": B64.encode(&sig) }] }).to_string();
        assert_eq!(e.note_redeem("acct-c", &wrong, now)["final"], true);
        // The same blinded message again signs to the same bytes; a different one for the
        // same payment and month is what `note_mint` refuses.
        assert_eq!(core_notes::blind_sign(&e.notes.key(cur).unwrap().sk, &blinded).unwrap(), sig_b);
        let (other, _) = core_notes::blind(&pk, &note, &[8u8; 32]).unwrap();
        assert_ne!(core_notes::blinded_fingerprint(&other), fp);
    }

    /// The book is the only record that a note was spent. A book restored from a backup
    /// may not have that record: then the same note, presented again by the app, buys
    /// the month again — once — and a book that does have it answers "again".
    #[tokio::test]
    async fn a_spent_note_buys_its_month_again_only_where_the_book_forgot_it() {
        let now = tokumai_core::subscription::ms_from_civil(2026, 10, 5);
        let cur = core_notes::epoch_of_ms(now);
        let seed = [4u8; 32];
        let note = core_notes::note_for(&seed, 1, cur);
        // Two enclaves on the same data key: the same month keys, two books.
        let first = enclave();
        let pk = core_notes::public_from_spki(&first.notes.published(now).unwrap()[1].1).unwrap();
        let (blinded, secret) = core_notes::blind(&pk, &note, &seed).unwrap();
        let sig = core_notes::finalize(&pk, &core_notes::blind_sign(&first.notes.key(cur).unwrap().sk, &blinded).unwrap(), &secret, &note).unwrap();
        let body = json!({ "notes": [{ "note": B64.encode(note.to_bytes()), "sig": B64.encode(&sig) }] }).to_string();
        assert_eq!(first.note_redeem("acct-a", &body, now)["granted"], 1);
        assert_eq!(first.note_redeem("acct-a", &body, now + 1)["again"], 1, "the book that has the record grants nothing");
        // The book restored from before the redemption: no record, the note is honoured once.
        let restored = enclave();
        let r = restored.note_redeem("acct-a", &body, now + 2);
        assert_eq!((r["granted"].as_u64(), r["plan"]["active"].as_bool()), (Some(1), Some(true)), "{r}");
        assert_eq!(restored.note_redeem("acct-a", &body, now + 3)["again"], 1);
    }

    #[tokio::test]
    async fn minting_through_the_operation_refuses_what_it_should() {
        let e = enclave();
        let now = tokumai_core::subscription::ms_from_civil(2026, 10, 5);
        // No proof at all: final, so the app stops asking.
        let r = e.note_mint(&json!({ "epoch": 9, "blinded": B64.encode([1u8; 256]) }).to_string(), now).await;
        assert_eq!(r["final"], true, "{r}");
        // Not an App Store transaction: final as well.
        let r = e.note_mint(&json!({ "epoch": 9, "blinded": B64.encode([1u8; 256]), "proof": { "kind": "apple", "jws": "a.b.c" } }).to_string(), now).await;
        assert_eq!(r["final"], true, "{r}");
    }
}
