//! Stripe, for plans bought outside the App Store. Carried over from the first server with
//! the fixes of the audit of 2026-09-21: the billing cycle starts on the day of purchase
//! (no anchor to the 1st, no part-month to prorate — H2), a plan is changed in place rather
//! than bought twice (H3), and money that goes back is read off the customer's charges (M1).
//!
//! The secret key lives in the enclave (sealed), so the operator's machine never holds it.
//! What reaches Stripe about an account: nothing. The checkout carries a random reference;
//! the link between a Stripe subscription and an account exists only inside.

use crate::secrets::SecretSource;
use serde_json::Value;
use tokumai_core::subscription::TIERS;

const API: &str = "https://api.stripe.com/v1";
/// How long a checkout link stays open.
const CHECKOUT_MINUTES: u64 = 30;

pub struct Stripe {
    secret_key: String,
    /// Where Stripe sends the browser after paying (a page that says "back to the app").
    redirect_url: String,
    /// The six plan prices at Stripe: the three monthly tiers, then the three yearly.
    price_ids: Vec<String>,
}

/// What the app's subscription looks like at Stripe right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubState {
    pub tier: usize,
    pub yearly: bool,
    pub paid: bool,
    pub start_ms: u64,
    pub end_ms: u64,
    pub back: MoneyBack,
}

/// Whether money for a plan went back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoneyBack {
    None,
    /// The charge for the CURRENT period was refunded in full. A partial refund is goodwill
    /// and leaves the plan running.
    Refunded,
    /// A chargeback on any recent charge.
    Disputed,
}

enum Fail {
    RateLimited(Option<u64>),
    Other(String),
}

impl From<Fail> for String {
    fn from(f: Fail) -> String {
        match f {
            Fail::RateLimited(Some(n)) => format!("the card processor is limiting requests right now — please try again in about {n} seconds"),
            Fail::RateLimited(None) => "the card processor is busy right now — please try again in a minute".into(),
            Fail::Other(s) => s,
        }
    }
}

fn safe_id(id: &str, prefix: &str) -> bool {
    id.starts_with(prefix) && id.len() < 100 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

pub fn is_stripe_url(u: &str) -> bool {
    u.strip_prefix("https://").map(|rest| rest.split(['/', '?', '#']).next() == Some("checkout.stripe.com")).unwrap_or(false)
}

/// Validate one price as Stripe returns it; its amount in cents.
pub fn plan_price_cents(v: &Value, interval: &str) -> Result<u64, String> {
    if v.get("active").and_then(|a| a.as_bool()) != Some(true) {
        return Err("is archived".into());
    }
    let cur = v.get("currency").and_then(|c| c.as_str()).unwrap_or("");
    if !cur.eq_ignore_ascii_case("eur") {
        return Err(format!("is in {cur}, plans are sold in EUR"));
    }
    let got = v.pointer("/recurring/interval").and_then(|i| i.as_str()).unwrap_or("one-off");
    let count = v.pointer("/recurring/interval_count").and_then(|c| c.as_u64()).unwrap_or(1);
    if got != interval || count != 1 {
        return Err(format!("renews every {count} {got}, this slot is for one {interval}"));
    }
    v.get("unit_amount").and_then(|a| a.as_u64()).filter(|a| *a > 0).ok_or_else(|| "has no fixed amount".into())
}

/// Read `GET /v1/charges?customer=…`: a chargeback anywhere, or this period's charge
/// refunded in full. Fields used (`created`, `refunded`, `disputed`) are stable across
/// API versions.
pub fn money_back(list: &Value, period_start_ms: u64) -> MoneyBack {
    let charges = list.get("data").and_then(|d| d.as_array()).map(Vec::as_slice).unwrap_or(&[]);
    if charges.iter().any(|c| c.get("disputed").and_then(|d| d.as_bool()) == Some(true)) {
        return MoneyBack::Disputed;
    }
    let day = 86_400_000u64;
    let refunded_now = charges.iter().any(|c| {
        let at = c.get("created").and_then(|t| t.as_u64()).unwrap_or(0) * 1000;
        at + day >= period_start_ms && at <= period_start_ms + day && c.get("refunded").and_then(|r| r.as_bool()) == Some(true)
    });
    if refunded_now {
        MoneyBack::Refunded
    } else {
        MoneyBack::None
    }
}

fn period_ms(v: &Value, field: &str) -> u64 {
    v.pointer(&format!("/items/data/0/{field}"))
        .and_then(|e| e.as_u64())
        .or_else(|| v.get(field).and_then(|e| e.as_u64()))
        .map(|s| s * 1000)
        .unwrap_or(0)
}

impl Stripe {
    /// Configured when all three are there; otherwise plans are not sold by card.
    pub fn from_secrets(s: &dyn SecretSource) -> Option<Stripe> {
        let secret_key = s.get("STRIPE_SECRET_KEY")?;
        let redirect_url = s.get("STRIPE_REDIRECT_URL")?;
        let price_ids: Vec<String> = s.get("STRIPE_PRICES")?.split(',').map(|p| p.trim().to_string()).filter(|p| safe_id(p, "price_")).collect();
        (price_ids.len() == TIERS.len() * 2).then_some(Stripe { secret_key, redirect_url, price_ids })
    }

    pub fn price_for(&self, tier: usize, yearly: bool) -> Option<&str> {
        self.price_ids.get(tier + if yearly { TIERS.len() } else { 0 }).map(String::as_str)
    }

    pub fn tier_of_price(&self, price_id: &str) -> Option<(usize, bool)> {
        let at = self.price_ids.iter().position(|p| p == price_id)?;
        Some((at % TIERS.len(), at >= TIERS.len()))
    }

    async fn call(&self, req: reqwest::RequestBuilder) -> Result<Value, Fail> {
        let res = req
            .basic_auth(&self.secret_key, Some(""))
            .timeout(std::time::Duration::from_secs(20))
            .send()
            .await
            .map_err(|_| Fail::Other("the card processor could not be reached".into()))?;
        let status = res.status();
        let retry = res.headers().get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse().ok());
        let body: Value = res.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(body);
        }
        eprintln!("tokumai-enclave: stripe {status}");
        Err(match status.as_u16() {
            429 => Fail::RateLimited(retry),
            401 | 403 => Fail::Other("the card processor refused the operator's key".into()),
            404 => Fail::Other("the card processor does not know that object".into()),
            _ => Fail::Other(format!("card processor {status}: {}", body.pointer("/error/message").and_then(|m| m.as_str()).unwrap_or("").chars().take(160).collect::<String>())),
        })
    }

    /// A checkout for a plan. Billing starts the day it is paid and renews on that day.
    /// `consent` is the version of the withdrawal confirmations the person gave (§ 356 BGB),
    /// kept with the subscription so the sale can show it.
    pub async fn create_subscription(&self, tier: usize, yearly: bool, reference: &str, consent: &str) -> Result<(String, String, u64), String> {
        let price = self.price_for(tier, yearly).ok_or("this plan is not sold by card")?.to_string();
        let expires = crate::now_ms() / 1000 + CHECKOUT_MINUTES * 60;
        let expires_s = expires.to_string();
        let form: Vec<(&str, &str)> = vec![
            ("mode", "subscription"),
            ("line_items[0][price]", &price),
            ("line_items[0][quantity]", "1"),
            ("client_reference_id", reference),
            ("metadata[orderId]", reference),
            ("subscription_data[metadata][orderId]", reference),
            ("subscription_data[metadata][consent]", consent),
            ("billing_address_collection", "required"),
            ("adaptive_pricing[enabled]", "false"),
            ("locale", "en"),
            ("expires_at", &expires_s),
            ("success_url", &self.redirect_url),
        ];
        let v = self.call(crate::http::client().post(format!("{API}/checkout/sessions")).header("Idempotency-Key", reference).form(&form)).await?;
        let id = v.get("id").and_then(|i| i.as_str()).filter(|i| safe_id(i, "cs_")).ok_or("no checkout session came back")?;
        let url = v.get("url").and_then(|u| u.as_str()).filter(|u| is_stripe_url(u)).ok_or("no checkout link came back")?;
        Ok((id.to_string(), url.to_string(), expires * 1000))
    }

    /// The subscription a checkout created, once it is complete AND paid; None before.
    pub async fn checkout_subscription(&self, session: &str) -> Result<Option<String>, String> {
        if !safe_id(session, "cs_") {
            return Err("not a checkout session".into());
        }
        let v = match self.call(crate::http::client().get(format!("{API}/checkout/sessions/{session}"))).await {
            Ok(v) => v,
            Err(Fail::RateLimited(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if v.get("status").and_then(|s| s.as_str()) != Some("complete") || v.get("payment_status").and_then(|s| s.as_str()) != Some("paid") {
            return Ok(None);
        }
        Ok(v.get("subscription").and_then(|x| x.as_str()).filter(|s| safe_id(s, "sub_")).map(str::to_string))
    }

    /// The subscription as Stripe has it now — tier and period read from Stripe's own record,
    /// never from the app — and whether money for it went back.
    pub async fn subscription(&self, sub_id: &str) -> Result<SubState, String> {
        if !safe_id(sub_id, "sub_") {
            return Err("not a subscription id".into());
        }
        let v = self.call(crate::http::client().get(format!("{API}/subscriptions/{sub_id}"))).await?;
        let price = v.pointer("/items/data/0/price/id").and_then(|p| p.as_str()).ok_or("the subscription names no price")?;
        let (tier, yearly) = self.tier_of_price(price).ok_or("the subscription is for a plan not sold here")?;
        let start_ms = period_ms(&v, "current_period_start");
        let back = match v.get("customer").and_then(|c| c.as_str()).filter(|c| safe_id(c, "cus_")) {
            Some(cus) => match self.call(crate::http::client().get(format!("{API}/charges")).query(&[("customer", cus), ("limit", "10")])).await {
                Ok(list) => money_back(&list, start_ms),
                Err(_) => MoneyBack::None, // nothing known is not a revocation
            },
            None => MoneyBack::None,
        };
        let paid = matches!(v.get("status").and_then(|s| s.as_str()), Some("active" | "trialing"));
        Ok(SubState { tier, yearly, paid, start_ms, end_ms: period_ms(&v, "current_period_end"), back })
    }

    /// Change a running subscription to another tier of the same interval. An upgrade is
    /// charged at once, pro rata, and refused whole if the charge fails or needs the bank's
    /// confirmation; a downgrade takes effect with the next period.
    ///
    /// `reference` is the idempotency key's stem: a retry after a lost answer is the same
    /// request to Stripe, never a second charge. The subscription's latest invoice goes
    /// into the key too, so that the SAME change asked for again after it was undone
    /// (up, down, up) is a new request — Stripe would otherwise replay the first answer
    /// for a day and charge nothing while the ledger took it as applied (audit H3). And
    /// what Stripe applied is read back fresh, never from the answer to the POST, which a
    /// replay repeats verbatim.
    pub async fn change_subscription(&self, sub_id: &str, tier: usize, yearly: bool, upgrade: bool, reference: &str) -> Result<usize, String> {
        if !safe_id(sub_id, "sub_") {
            return Err("not a subscription id".into());
        }
        let price = self.price_for(tier, yearly).ok_or("that plan is not sold by card")?.to_string();
        let cur = self.call(crate::http::client().get(format!("{API}/subscriptions/{sub_id}"))).await?;
        let item = cur.pointer("/items/data/0/id").and_then(|i| i.as_str()).filter(|i| safe_id(i, "si_")).ok_or("the subscription names no item")?.to_string();
        let invoice = cur.get("latest_invoice").and_then(|i| i.as_str()).filter(|i| safe_id(i, "in_")).unwrap_or("none");
        let reference = format!("{reference}-{invoice}");
        let form: Vec<(&str, &str)> = vec![
            ("items[0][id]", &item),
            ("items[0][price]", &price),
            ("proration_behavior", if upgrade { "always_invoice" } else { "none" }),
            ("payment_behavior", if upgrade { "error_if_incomplete" } else { "allow_incomplete" }),
        ];
        self.call(crate::http::client().post(format!("{API}/subscriptions/{sub_id}")).header("Idempotency-Key", &reference).form(&form))
            .await
            .map_err(|e| match e {
                Fail::Other(m) if upgrade => format!("the upgrade could not be charged, so nothing changed ({m})"),
                e => e.into(),
            })?;
        let applied = self.call(crate::http::client().get(format!("{API}/subscriptions/{sub_id}"))).await?;
        let now = applied.pointer("/items/data/0/price/id").and_then(|p| p.as_str()).unwrap_or("");
        match self.tier_of_price(now) {
            Some((t, y)) if (t, y) == (tier, yearly) => Ok(t),
            _ => Err("the card processor did not apply the change".into()),
        }
    }

    /// End a subscription now — after a chargeback, so nothing more is charged.
    pub async fn cancel_now(&self, sub_id: &str) -> Result<(), String> {
        if !safe_id(sub_id, "sub_") {
            return Err("not a subscription id".into());
        }
        self.call(crate::http::client().delete(format!("{API}/subscriptions/{sub_id}"))).await.map(|_| ()).map_err(String::from)
    }

    /// What the six plans cost at Stripe, in cents, in price order — the ladder the app
    /// shows is read from where the money is charged, never typed twice.
    pub async fn plan_prices(&self) -> Result<Vec<u64>, String> {
        let mut out = Vec::with_capacity(self.price_ids.len());
        for (i, id) in self.price_ids.iter().enumerate() {
            let v = self.call(crate::http::client().get(format!("{API}/prices/{id}"))).await?;
            let want = if i < TIERS.len() { "month" } else { "year" };
            out.push(plan_price_cents(&v, want).map_err(|e| format!("price #{} {e}", i + 1))?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn money_back_is_a_full_refund_of_this_period_or_any_chargeback() {
        let start = 1_789_000_000_000u64;
        let at = start / 1000;
        let l = |v: Value| json!({ "data": v });
        assert_eq!(money_back(&l(json!([{ "created": at, "refunded": false }])), start), MoneyBack::None);
        assert_eq!(money_back(&l(json!([{ "created": at, "refunded": true }])), start), MoneyBack::Refunded);
        assert_eq!(money_back(&l(json!([{ "created": at, "refunded": false, "amount_refunded": 300 }])), start), MoneyBack::None, "partial = goodwill");
        assert_eq!(money_back(&l(json!([{ "created": at - 30 * 86_400, "refunded": true }])), start), MoneyBack::None, "last month's");
        assert_eq!(money_back(&l(json!([{ "created": at }, { "created": at - 90 * 86_400, "disputed": true }])), start), MoneyBack::Disputed);
        assert_eq!(money_back(&json!({}), start), MoneyBack::None);
    }

    #[test]
    fn a_price_is_checked_before_it_is_sold() {
        let ok = json!({ "active": true, "currency": "eur", "unit_amount": 1000, "recurring": { "interval": "month", "interval_count": 1 } });
        assert_eq!(plan_price_cents(&ok, "month"), Ok(1000));
        assert!(plan_price_cents(&ok, "year").is_err(), "a monthly price in a yearly slot");
        let usd = json!({ "active": true, "currency": "usd", "unit_amount": 1000, "recurring": { "interval": "month" } });
        assert!(plan_price_cents(&usd, "month").unwrap_err().contains("EUR"));
        let archived = json!({ "active": false, "currency": "eur", "unit_amount": 1000, "recurring": { "interval": "month" } });
        assert!(plan_price_cents(&archived, "month").is_err());
    }

    #[test]
    fn only_stripes_hosted_checkout_is_a_link() {
        assert!(is_stripe_url("https://checkout.stripe.com/c/pay/cs_test_1"));
        assert!(!is_stripe_url("https://checkout.stripe.com.evil.example/"));
        assert!(!is_stripe_url("http://checkout.stripe.com/"));
        assert!(!is_stripe_url("https://evil.example/?checkout.stripe.com"));
    }

    #[test]
    fn prices_map_to_tiers_and_back() {
        let s = Stripe { secret_key: "k".into(), redirect_url: "u".into(), price_ids: (0..6).map(|i| format!("price_{i}")).collect() };
        assert_eq!(s.price_for(1, true), Some("price_4"));
        assert_eq!(s.tier_of_price("price_4"), Some((1, true)));
        assert_eq!(s.tier_of_price("price_x"), None);
    }
}
