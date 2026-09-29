//! The plan operations the app calls, and the renewal check the host drives. All of it
//! inside: the account, its plan and the Stripe or Apple reference meet nowhere else.
//!
//! - `plans` — the ladder: tiers, allowances, prices (read from Stripe), product ids;
//! - `plan.create` / `plan.status` — a card checkout (Stripe), and whether it was paid;
//! - `plan.change` — a card plan to another tier, in place;
//! - `iap.verify` — an App Store transaction the app hands over: a plan, a renewal, a
//!   refund, or prepaid credit;
//! - [`Enclave::tick`] — every 30 s from the host: periods roll, and one plan per rail is
//!   asked about (renewal, refund, chargeback), stalest first, each about every six hours.

use crate::apple;
use crate::plans::{self, Plan};
use crate::service::{error, Enclave};
use serde_json::{json, Value};
use tokumai_core::subscription::TIERS;

/// How stale a plan must be before the renewal check asks its rail again.
const STALE_MS: u64 = 6 * 3_600_000;
/// How often the prices are read again from Stripe.
#[allow(dead_code)] // kept for the plan sheet's own staleness check
const PRICES_EVERY_MS: u64 = 3_600_000;
/// The consent text version the app shows beside the two confirmations (§ 356 (5) BGB).
const CONSENT_VERSIONS: &[&str] = &["2026-09-22"];

fn final_error(msg: &str) -> Value {
    // "final": the app finishes this App Store transaction instead of re-sending it forever.
    json!({ "kind": "error", "error": msg, "final": true })
}

fn body_of(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or(Value::Null)
}

impl Enclave {
    pub(crate) fn plan_summary(&self, account: &str) -> Value {
        let Ok(l) = self.ledger.lock() else { return Value::Null };
        let Ok(Some(p)) = plans::plan_of(&l, account) else { return Value::Null };
        let now = crate::now_ms();
        let (granted, left, ends) = l
            .allowance_of(&l.acct_key(account))
            .ok()
            .flatten()
            .map(|(_, ends, g, left)| (g, left, ends))
            .unwrap_or((0, 0, 0));
        json!({
            "tier": p.tier, "yearly": p.yearly, "rail": if p.is_app_store() { "appstore" } else { "stripe" },
            "active": p.paid_at(now), "paidUntil": p.paid_until_ms, "periodEnd": ends,
            "granted": granted, "left": left, "tokuPerMonth": TIERS[p.tier.min(TIERS.len() - 1)].0,
        })
    }

    pub(crate) fn plans_op(&self, account: &str, _now: u64) -> Value {
        let prices = self.plan_prices.lock().map(|p| p.0.clone()).unwrap_or_default();
        let n = TIERS.len();
        let tiers: Vec<Value> = TIERS
            .iter()
            .enumerate()
            .map(|(i, (toku, cents))| {
                json!({ "tier": i, "toku": toku, "cents": cents,
                        "web": (prices.len() == n * 2).then(|| json!({ "month": prices[i], "year": prices[i + n] })) })
            })
            .collect();
        json!({
            "kind": "plans", "tiers": tiers, "byCard": self.stripe.is_some(),
            "appStore": { "plans": apple::plan_ids(), "credit": apple::product_ids() },
            "plan": self.plan_summary(account), "consentVersion": CONSENT_VERSIONS[CONSENT_VERSIONS.len() - 1],
        })
    }

    pub(crate) async fn plan_create(&self, account: &str, body: &str, now: u64) -> Value {
        let Some(stripe) = &self.stripe else { return error("plans are not sold by card here") };
        let b = body_of(body);
        let tier = b.get("tier").and_then(|t| t.as_u64()).unwrap_or(0) as usize;
        let yearly = b.get("yearly").and_then(|y| y.as_bool()).unwrap_or(false);
        if tier >= TIERS.len() {
            return error("no such plan");
        }
        // Both confirmations, and which text they were given to (audit M8): without them the
        // right of withdrawal does not lapse, and no order may be placed.
        let c = b.get("consent").cloned().unwrap_or(Value::Null);
        let version = c.get("version").and_then(|v| v.as_str()).unwrap_or("");
        let both = c.get("immediateStart").and_then(|v| v.as_bool()) == Some(true) && c.get("waiverAck").and_then(|v| v.as_bool()) == Some(true);
        if !both || !CONSENT_VERSIONS.contains(&version) {
            return error("both confirmations are needed before a plan can be ordered");
        }
        match self.ledger.lock().map_err(|_| "ledger unavailable".to_string()).and_then(|l| plans::has_paid_plan(&l, account, now)) {
            Ok(true) => return error(plans::PLAN_ALREADY),
            Ok(false) => {}
            Err(e) => return error(&e),
        }
        let reference = format!("plan{}", hex::encode(rand::random::<[u8; 12]>()));
        match stripe.create_subscription(tier, yearly, &reference, version).await {
            Ok((session, checkout, expires)) => json!({ "kind": "plan.open", "session": session, "checkout": checkout, "expiresAt": expires }),
            Err(e) => error(&e),
        }
    }

    pub(crate) async fn plan_status(&self, account: &str, body: &str, now: u64) -> Value {
        let Some(stripe) = &self.stripe else { return error("plans are not sold by card here") };
        let session = body_of(body).get("session").and_then(|s| s.as_str()).unwrap_or("").to_string();
        let sub = match stripe.checkout_subscription(&session).await {
            Ok(Some(sub)) => sub,
            Ok(None) => return json!({ "kind": "plan.pending" }),
            Err(e) => return error(&e),
        };
        let st = match stripe.subscription(&sub).await {
            Ok(st) if st.paid => st,
            Ok(_) => return json!({ "kind": "plan.pending" }),
            Err(e) => return error(&e),
        };
        let rail = format!("stripe:{sub}");
        let done = self
            .ledger
            .lock()
            .map_err(|_| "ledger unavailable".to_string())
            .and_then(|l| plans::subscribe_or_renew(&l, account, st.tier, st.yearly, &rail, now, st.start_ms, st.end_ms));
        // From here the account's next question is the one that could be tied to the
        // payment we just took. `cover` watches until it comes (see that module).
        if done.is_ok() {
            self.cover.paid(&self.account_key(account), now);
        }
        match done {
            Ok(_) => json!({ "kind": "plan.paid", "plan": self.plan_summary(account) }),
            Err(e) => error(&e),
        }
    }

    pub(crate) async fn plan_change(&self, account: &str, body: &str, now: u64) -> Value {
        let Some(stripe) = &self.stripe else { return error("plans are not sold by card here") };
        let b = body_of(body);
        let tier = (b.get("tier").and_then(|t| t.as_u64()).unwrap_or(0) as usize).min(TIERS.len() - 1);
        let yearly = b.get("yearly").and_then(|y| y.as_bool()).unwrap_or(false);
        let (plan, period): (Plan, u32) = match self.ledger.lock().map_err(|_| "ledger unavailable".to_string()).and_then(|l| {
            let p = plans::plan_of(&l, account)?;
            let period = l.allowance_of(&l.acct_key(account))?.map(|(p, ..)| p).unwrap_or(0);
            Ok(p.map(|p| (p, period)))
        }) {
            Ok(Some(x)) => x,
            Ok(None) => return error("this account has no plan to change"),
            Err(e) => return error(&e),
        };
        if !plan.paid_at(now) {
            return error("this plan is not paid for right now, so it cannot be changed");
        }
        if plan.is_app_store() {
            return error("this plan is billed through the App Store — change it in the App Store");
        }
        if plan.yearly != yearly {
            return error("switching between monthly and yearly is not a change of plan: cancel this one, and subscribe to the other once its period has ended");
        }
        if plan.tier == tier {
            return error("that is already your plan");
        }
        let sub = plan.rail.trim_start_matches("stripe:").to_string();
        let upgrade = TIERS[tier].0 > TIERS[plan.tier].0;
        // One key per (subscription, target, period): a retry after a lost answer is the
        // same request to Stripe, never a second charge.
        let reference = format!("chg-{sub}-{tier}-{period}");
        if let Err(e) = stripe.change_subscription(&sub, tier, yearly, upgrade, &reference).await {
            return error(&e);
        }
        match self.ledger.lock().map_err(|_| "ledger unavailable".to_string()).and_then(|l| plans::change_tier(&l, account, tier, now)) {
            Ok(added) => json!({ "kind": "plan.changed", "added": added, "plan": self.plan_summary(account) }),
            Err(e) => error(&e),
        }
    }

    pub(crate) fn iap_verify(&self, account: &str, body: &str, now: u64) -> Value {
        let b = body_of(body);
        let jws = b.get("jws").and_then(|j| j.as_str()).unwrap_or("");
        // Only "Restore purchases" may move an App Store plan here from another account; the
        // report every launch makes never does (two accounts under one Apple ID would pass
        // the plan back and forth).
        let restore = b.get("restore").and_then(|r| r.as_bool()) == Some(true);
        let tx = match apple::verify_jws(jws, now) {
            Ok(tx) => tx,
            Err(e) => return final_error(&e),
        };
        self.iap_apply(account, &tx, restore, now)
    }

    /// What a verified App Store transaction does to the account: a plan begins or renews,
    /// or a credit tile is added. Apart from `iap_verify` only the tests call this — with
    /// a transaction they made up, since nobody can sign one of Apple's.
    ///
    /// The ledger is locked for the whole of it. NOTHING in here may take that lock again
    /// (`self.account_key`, `self.balance`, `self.plan_summary`…): a std mutex taken twice
    /// by the same thread waits for ever, and with the runtime's other worker waiting for
    /// the same lock the enclave answers nothing more — which it did, for three days, and
    /// again the next time a purchase went through (2026-09-25, -28). Use `l` instead.
    pub(crate) fn iap_apply(&self, account: &str, tx: &apple::AppleTx, restore: bool, now: u64) -> Value {
        let Ok(l) = self.ledger.lock() else { return error("ledger unavailable") };
        if apple::revoked_plan(tx) {
            let _ = plans::revoke(&l, &format!("iap:{}", tx.original_transaction_id), false, now);
            return final_error("this plan was refunded by Apple and has ended");
        }
        // A subscription that is refused stays a subscription: reporting the credit path's
        // reason for it says "this purchase is not a credit product", which answers a
        // question nobody asked and hides the real one. Seen from a phone on 2026-09-24,
        // where the true answer was "sandbox purchases are not accepted by this server".
        if tx.kind == "Auto-Renewable Subscription" {
            if let Err(why) = apple::plan_for(tx, now) {
                return final_error(&why);
            }
        }
        if let Ok((tier, yearly)) = apple::plan_for(tx, now) {
            let rail = format!("iap:{}", tx.original_transaction_id);
            let mine = l.acct_key(account);
            match l.rail_owner(&rail) {
                Ok(Some(owner)) if owner != mine && restore => match plans::move_plan(&l, &rail, account, now) {
                    Ok(true) => {}
                    Ok(false) => return error(plans::PLAN_ALREADY),
                    Err(e) => return error(&e),
                },
                Ok(Some(owner)) if owner != mine => {
                    return json!({ "kind": "error", "otherAccount": true,
                        "error": "this App Store plan belongs to another tokumai account. To use one plan on several devices, use the same recovery phrase on each; to bring it to this account, tap Restore purchases" });
                }
                Err(e) => return error(&e),
                _ => {}
            }
            // `l` is held here: `self.account_key` would take the ledger lock a second time,
            // and a std mutex taken twice by one thread waits for ever. That was the
            // enclave that went deaf whenever a purchase went THROUGH (2026-09-25, -28).
            self.cover.paid(&l.acct_key(account), now);
            return match plans::subscribe_or_renew(&l, account, tier, yearly, &rail, now, tx.purchased_at_ms, tx.expires_at_ms) {
                Ok(_) => {
                    drop(l);
                    json!({ "kind": "iap.ok", "plan": self.plan_summary(account) })
                }
                Err(e) => error(&e),
            };
        }
        match apple::credit_for(tx) {
            Ok(toku) => {
                let first = l.first_payment(&format!("apple-tx:{}", tx.transaction_id), now);
                let credited = match first {
                    Ok(true) => match l.credit_prepaid(account, toku, now) {
                        Ok(()) => {
                            // `l` is held here: `self.account_key` would take the ledger lock a second time,
            // and a std mutex taken twice by one thread waits for ever. That was the
            // enclave that went deaf whenever a purchase went THROUGH (2026-09-25, -28).
            self.cover.paid(&l.acct_key(account), now);
                            toku
                        }
                        Err(e) => return error(&e),
                    },
                    Ok(false) => 0, // a resend: credited the first time
                    Err(e) => return error(&e),
                };
                let total = l.balance(account, now).map(|b| b.total).unwrap_or(0);
                json!({ "kind": "iap.ok", "credited": credited, "balance": total })
            }
            Err(e) => final_error(&e),
        }
    }

    /// The host calls this every 30 seconds.
    pub async fn tick(&self) {
        let now = crate::now_ms();
        if let Ok(l) = self.ledger.lock() {
            match plans::roll(&l, now) {
                Ok(0) => {}
                Ok(n) => eprintln!("tokumai-enclave: {n} plan allowance(s) began or lapsed a period"),
                Err(_) => eprintln!("tokumai-enclave: the period roll failed"),
            }
        }
        // One call to Stripe every tick, whether or not there is anything to ask about.
        //
        // Our host sees the enclave's traffic, and a call to Stripe that happens only when
        // something happened says when something happened — which, next to the moment a
        // question goes out, is half of a join between a named customer and a question.
        // A steady beat says nothing: there is always a call, and it always looks alike.
        // It does not protect against us (we hold the merchant records either way); it
        // protects against whoever else holds the machine.
        let stale = self.ledger.lock().ok().and_then(|l| plans::stale(&l, now, STALE_MS).ok()).unwrap_or_default();
        let checked = match stale.iter().find(|(_, p)| !p.is_app_store()) {
            Some((key, plan)) => {
                self.check_stripe(key, plan, now).await;
                true
            }
            None => false,
        };
        if let Some(stripe) = &self.stripe {
            if !checked {
                // Nothing needed asking, so ask for the prices instead: a real call, on
                // the same beat, that keeps the plan sheet current as a side effect.
                match stripe.plan_prices().await {
                    Ok(prices) => {
                        if let Ok(mut p) = self.plan_prices.lock() {
                            *p = (prices, now);
                        }
                    }
                    Err(_) => {
                        if let Ok(mut p) = self.plan_prices.lock() {
                            p.1 = now;
                        }
                    }
                }
            }
        }
        if let Some((key, plan)) = stale.iter().find(|(_, p)| p.is_app_store()) {
            self.check_apple(key, plan, now).await;
        }
    }

    async fn check_stripe(&self, key: &str, plan: &Plan, now: u64) {
        let Some(stripe) = &self.stripe else { return };
        let sub = plan.rail.trim_start_matches("stripe:");
        match stripe.subscription(sub).await {
            Ok(st) => {
                let back = st.back;
                if back != crate::stripe::MoneyBack::None {
                    let disputed = back == crate::stripe::MoneyBack::Disputed;
                    if disputed {
                        // Stop charging a card whose owner says the payments were not theirs.
                        let _ = stripe.cancel_now(sub).await;
                    }
                    if let Ok(l) = self.ledger.lock() {
                        let _ = plans::revoke(&l, &plan.rail, disputed, now);
                    }
                    eprintln!("tokumai-enclave: a card plan ended at once — {}", if disputed { "a chargeback" } else { "refunded in full" });
                    return;
                }
                if let Ok(l) = self.ledger.lock() {
                    let until = if st.paid { st.end_ms } else { 0 };
                    if plans::renew_by_rail(&l, &plan.rail, st.tier, st.yearly, now, st.start_ms, until).is_err() {
                        let _ = plans::mark_checked(&l, key, now);
                    }
                }
            }
            Err(_) => {
                if let Ok(l) = self.ledger.lock() {
                    let _ = plans::mark_checked(&l, key, now);
                }
            }
        }
    }

    async fn check_apple(&self, key: &str, plan: &Plan, now: u64) {
        let Some(api) = &self.apple_api else {
            if let Ok(l) = self.ledger.lock() {
                let _ = plans::mark_checked(&l, key, now);
            }
            return;
        };
        let otid = plan.rail.trim_start_matches("iap:").to_string();
        let answer = apple::subscription_status(api, &otid).await;
        let Ok(l) = self.ledger.lock() else { return };
        let applied = match answer {
            Ok(Some((status, jws))) => match apple::verify_jws(&jws, now) {
                Ok(tx) if tx.original_transaction_id == otid => {
                    if apple::revoked_plan(&tx) || status == 5 {
                        plans::revoke(&l, &plan.rail, false, now).is_ok()
                    } else if let Some((tier, yearly)) = apple::product_plan(&tx.product_id) {
                        plans::renew_by_rail(&l, &plan.rail, tier, yearly, now, tx.purchased_at_ms, tx.expires_at_ms).is_ok()
                    } else {
                        false
                    }
                }
                _ => false,
            },
            _ => false,
        };
        if !applied {
            let _ = plans::mark_checked(&l, key, now);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::apple::AppleTx;
    use crate::service::{Db, Enclave, Platform};
    use std::time::Duration;

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
        })
        .unwrap();
        Box::leak(Box::new(e))
    }

    fn tx(product: &str, kind: &str, expires_at_ms: u64) -> AppleTx {
        AppleTx {
            bundle_id: "com.tokumai.app".into(),
            product_id: product.into(),
            transaction_id: format!("2000000{product}"),
            original_transaction_id: format!("2000000{product}"),
            environment: "Production".into(),
            kind: kind.into(),
            quantity: 1,
            purchased_at_ms: 1_757_500_000_000,
            revoked: false,
            ownership: "PURCHASED".into(),
            storefront: "DEU".into(),
            expires_at_ms,
        }
    }

    /// `f` on a thread of its own, with a deadline: a deadlock is a test that never ends,
    /// and a test that never ends says nothing.
    fn within(secs: u64, f: impl FnOnce() -> serde_json::Value + Send + 'static) -> serde_json::Value {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(Duration::from_secs(secs)).expect("the transaction was still being applied when the deadline passed — the ledger lock is taken twice on one thread")
    }

    /// The enclave went deaf whenever a purchase went THROUGH: the successful branches
    /// took the ledger lock a second time while holding it. Both branches, with a deadline.
    #[test]
    fn a_purchase_that_goes_through_does_not_lock_the_enclave_up() {
        let e = enclave();
        let now = 1_757_600_000_000;
        let credited = within(10, move || e.iap_apply("acct-a", &tx("com.tokumai.app.credit.10", "Consumable", 0), false, now));
        assert_eq!(credited["kind"], "iap.ok", "{credited}");
        assert_eq!(credited["credited"], 10 * tokumai_core::billing::TOKU_PER_USD);
        let plan = within(10, move || e.iap_apply("acct-b", &tx("com.tokumai.app.plan.10", "Auto-Renewable Subscription", now + 86_400_000), false, now));
        assert_eq!(plan["kind"], "iap.ok", "{plan}");
        assert!(plan["plan"].is_object(), "{plan}");
        // And the enclave still answers afterwards.
        let again = within(10, move || e.iap_apply("acct-a", &tx("com.tokumai.app.credit.10", "Consumable", 0), false, now));
        assert_eq!(again["credited"], 0, "a resend is credited once: {again}");
    }
}
