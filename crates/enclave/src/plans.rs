//! Plans: monthly or yearly subscriptions, paid on Stripe or the App Store, each granting an
//! allowance per period. Carried over from the first server with the fixes of the audit of
//! 2026-09-21 built in:
//!
//! - a plan runs in its OWN months, from the day it was bought, on both rails; one payment is
//!   one allowance, identified by the period's start (H2, H5);
//! - one paid plan per account; a second rail is refused, and every rail stays bound to its
//!   account, so a replaced one cannot become a month on a fresh account (H3);
//! - money that goes back (a full refund, a chargeback) ends the plan at once and lapses its
//!   allowance (M1);
//! - each ended period keeps one coarse step of how much was drawn, for 120 days (180 after a
//!   chargeback) — the evidence a dispute needs and nothing more.
//!
//! Everything here works on the ledger's stored account names, never on account ids, so the
//! renewal check walking every plan does not hold a single id.

use crate::ledger::Ledger;
use serde::{Deserialize, Serialize};
use tokumai_core::subscription::{prorata_toku, slice_at, Allowance, TIERS};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// Index into `subscription::TIERS`.
    pub tier: usize,
    pub yearly: bool,
    /// `stripe:sub_…` or `iap:<original transaction id>` — needed to ask the rail about it.
    pub rail: String,
    /// The paid period the rail last reported: start and end.
    pub period_start_ms: u64,
    pub paid_until_ms: u64,
    /// What the rail last said. Paid means active AND inside the paid period.
    pub active: bool,
    /// When the rail was last asked; the renewal check asks the stalest first.
    pub checked_at_ms: u64,
    #[serde(default)]
    pub disputed_at_ms: u64,
    #[serde(default)]
    pub usage: Vec<PeriodUsage>,
}

impl Plan {
    pub fn paid_at(&self, now_ms: u64) -> bool {
        self.active && self.paid_until_ms > now_ms
    }
    pub fn is_app_store(&self) -> bool {
        self.rail.starts_with("iap:")
    }
    /// Paid with blind notes: the enclave holds no rail to ask about, by design. The app
    /// brings the next month's note itself.
    pub fn is_note(&self) -> bool {
        self.rail.starts_with("note:")
    }
}

/// One ended period, as a chargeback answer needs it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeriodUsage {
    /// The period's start (unix seconds).
    pub period: u32,
    pub tier: usize,
    /// "none" | "some" (under half) | "most" | "all" (95 % or more). Apple asks for
    /// consumption in steps like these.
    pub drawn: String,
}

pub const USAGE_KEEP_MS: u64 = 120 * 86_400_000;
pub const DISPUTE_KEEP_MS: u64 = 180 * 86_400_000;

pub const PLAN_ALREADY: &str =
    "this account already has a plan — change it instead of taking out a second one, or cancel it and subscribe again once its month has ended";
pub const PLAN_ELSEWHERE: &str = "this subscription is already on another tokumai account";

/// Record the period now ending, once.
fn close_period(l: &Ledger, key: &str, plan: &mut Plan) -> Result<(), String> {
    let Some((period, _, granted, left)) = l.allowance_of(key)? else { return Ok(()) };
    if period == 0 || granted == 0 || plan.usage.iter().any(|u| u.period == period) {
        return Ok(());
    }
    let drawn = granted.saturating_sub(left);
    let step = if drawn == 0 {
        "none"
    } else if drawn * 2 < granted {
        "some"
    } else if drawn * 100 < granted * 95 {
        "most"
    } else {
        "all"
    };
    plan.usage.push(PeriodUsage { period, tier: plan.tier, drawn: step.into() });
    Ok(())
}

fn prune_usage(plan: &mut Plan, now_ms: u64) -> bool {
    let before = plan.usage.len();
    let disputed = plan.disputed_at_ms > 0 && now_ms.saturating_sub(plan.disputed_at_ms) < DISPUTE_KEEP_MS;
    if !disputed {
        plan.usage.retain(|u| now_ms.saturating_sub(u.period as u64 * 1000) < USAGE_KEEP_MS);
    }
    plan.usage.len() != before
}

fn current_period(l: &Ledger, key: &str) -> Result<u32, String> {
    Ok(l.allowance_of(key)?.map(|(p, ..)| p).unwrap_or(0))
}

/// Why `rail` may not be attached to `account`, or None.
pub fn attach_refusal(l: &Ledger, account: &str, rail: &str, now_ms: u64) -> Result<Option<&'static str>, String> {
    attach_refusal_key(l, &l.acct_key(account), rail, now_ms)
}

fn attach_refusal_key(l: &Ledger, key: &str, rail: &str, now_ms: u64) -> Result<Option<&'static str>, String> {
    let key = key.to_string();
    if let Some(owner) = l.rail_owner(rail)? {
        if owner != key {
            return Ok(Some(PLAN_ELSEWHERE));
        }
    }
    Ok(match l.plan_get(&key)? {
        Some(p) if !p.rail.is_empty() && p.rail != rail && p.paid_at(now_ms) => Some(PLAN_ALREADY),
        _ => None,
    })
}

pub fn has_paid_plan(l: &Ledger, account: &str, now_ms: u64) -> Result<bool, String> {
    Ok(l.plan_get(&l.acct_key(account))?.is_some_and(|p| p.paid_at(now_ms)))
}

pub fn plan_of(l: &Ledger, account: &str) -> Result<Option<Plan>, String> {
    l.plan_get(&l.acct_key(account))
}

/// A plan starts, is confirmed, or renews — every sighting from either rail comes here, as
/// often as it likes: a period already granted is not granted again. `start`/`until` are the
/// rail's current paid period. `Ok(true)` when an allowance was handed out; `Err` carries a
/// refusal the person can act on.
#[allow(clippy::too_many_arguments)]
pub fn subscribe_or_renew(
    l: &Ledger,
    account: &str,
    tier: usize,
    yearly: bool,
    rail: &str,
    now_ms: u64,
    start_ms: u64,
    until_ms: u64,
) -> Result<bool, String> {
    renew_key(l, &l.acct_key(account), tier, yearly, rail, now_ms, start_ms, until_ms)
}

/// The renewal check's way in: the rail's word about a subscription, applied to whichever
/// account holds it — found by the rail, so no account id is ever at hand. Nothing happens
/// for a rail nobody holds.
#[allow(clippy::too_many_arguments)]
pub fn renew_by_rail(l: &Ledger, rail: &str, tier: usize, yearly: bool, now_ms: u64, start_ms: u64, until_ms: u64) -> Result<bool, String> {
    match l.rail_owner(rail)? {
        Some(key) => renew_key(l, &key, tier, yearly, rail, now_ms, start_ms, until_ms),
        None => Ok(false),
    }
}

/// A payment that was minted into a note pays that way and no other (`notes::mint_admission`
/// refuses the other direction).
pub const PLAN_MINTED: &str = "this payment was turned into a note and pays as one; it cannot also pay a plan directly";

#[allow(clippy::too_many_arguments)]
fn renew_key(l: &Ledger, key: &str, tier: usize, yearly: bool, rail: &str, now_ms: u64, start_ms: u64, until_ms: u64) -> Result<bool, String> {
    if let Some(why) = attach_refusal_key(l, key, rail, now_ms)? {
        return Err(why.into());
    }
    // One payment, one way (audit H2): a rail any of whose months was minted into a note
    // binds no plan — on this account or any other.
    if !rail.starts_with("note:") {
        for epoch in tokumai_core::notes::epochs_covered(start_ms, until_ms, yearly) {
            if l.minted_get(&crate::notes::mint_reference(rail, epoch))?.is_some() {
                return Err(PLAN_MINTED.into());
            }
        }
    }
    let key = key.to_string();
    let tier = tier.min(TIERS.len() - 1);
    let paid_now = until_ms > now_ms;
    let existing = l.plan_get(&key)?;
    if !paid_now && existing.is_none() {
        return Ok(false); // a rail reporting an ended period starts nothing
    }
    l.rail_bind(rail, &key)?;
    let mut plan = existing.unwrap_or_default();
    let was_known = plan.rail == rail;
    let old_tier = plan.tier;
    plan.tier = tier;
    plan.yearly = yearly;
    plan.rail = rail.to_string();
    plan.active = paid_now;
    plan.checked_at_ms = now_ms;
    plan.paid_until_ms = until_ms;
    if start_ms > 0 {
        plan.period_start_ms = start_ms;
    }
    let slice = slice_at(plan.period_start_ms, plan.paid_until_ms, plan.yearly, now_ms);
    let current = current_period(l, &key)?;
    let grant = slice.is_some_and(|(s, _)| Allowance::period_key(s) > current);
    match slice {
        Some((s, e)) if grant => {
            close_period(l, &key, &mut plan)?;
            l.allowance_set(&key, s, e, TIERS[tier].0)?;
        }
        Some((s, e)) if was_known && tier > old_tier => {
            let step = TIERS[tier].0 - TIERS[old_tier].0;
            l.allowance_add(&key, prorata_toku(step, e.saturating_sub(now_ms), e - s))?;
        }
        _ => {}
    }
    l.plan_put(&key, &plan)?;
    Ok(grant)
}

/// A web plan moved to another tier of the same interval (Stripe has billed it). An
/// upgrade adds the rest of the period pro rata; a downgrade starts with the next period.
pub fn change_tier(l: &Ledger, account: &str, tier: usize, now_ms: u64) -> Result<u64, String> {
    let key = l.acct_key(account);
    let Some(mut plan) = l.plan_get(&key)? else { return Ok(0) };
    let tier = tier.min(TIERS.len() - 1);
    let (old, new) = (TIERS[plan.tier].0, TIERS[tier].0);
    plan.tier = tier;
    l.plan_put(&key, &plan)?;
    if new <= old {
        return Ok(0);
    }
    let Some((s, e)) = slice_at(plan.period_start_ms, plan.paid_until_ms, plan.yearly, now_ms) else { return Ok(0) };
    let extra = prorata_toku(new - old, e.saturating_sub(now_ms), e - s);
    l.allowance_add(&key, extra)?;
    Ok(extra)
}

/// The beat, every minute: a paid period that has run out marks the plan unpaid; a new
/// period (a yearly plan's next month) is granted; an allowance whose period ended unrenewed
/// lapses; old usage steps go. Returns how many allowances began or lapsed.
pub fn roll(l: &Ledger, now_ms: u64) -> Result<usize, String> {
    let mut changed = 0;
    for key in l.plan_keys()? {
        let Some(mut plan) = l.plan_get(&key)? else { continue };
        let mut dirty = prune_usage(&mut plan, now_ms);
        if plan.active && plan.paid_until_ms <= now_ms {
            plan.active = false;
            dirty = true;
        }
        let slice = if plan.paid_at(now_ms) { slice_at(plan.period_start_ms, plan.paid_until_ms, plan.yearly, now_ms) } else { None };
        let allowance = l.allowance_of(&key)?;
        let current = allowance.map(|(p, ..)| p).unwrap_or(0);
        match slice {
            Some((s, e)) if Allowance::period_key(s) > current => {
                close_period(l, &key, &mut plan)?;
                l.allowance_set(&key, s, e, TIERS[plan.tier.min(TIERS.len() - 1)].0)?;
                changed += 1;
                dirty = true;
            }
            Some(_) => {}
            None => {
                if let Some((_, ends, granted, _)) = allowance {
                    if granted > 0 && now_ms >= ends {
                        close_period(l, &key, &mut plan)?;
                        l.allowance_lapse(&key)?;
                        changed += 1;
                        dirty = true;
                    }
                }
            }
        }
        if dirty {
            l.plan_put(&key, &plan)?;
        }
    }
    Ok(changed)
}

/// The money for the plan on `rail` went back (a full refund, a chargeback, Apple's refund):
/// it ends now and what is left of its allowance lapses. Coins do not exist any more, so
/// there is nothing on a device to recall. Returns whether a plan ended.
pub fn revoke(l: &Ledger, rail: &str, disputed: bool, now_ms: u64) -> Result<bool, String> {
    let Some(key) = l.rail_owner(rail)? else { return Ok(false) };
    let Some(mut plan) = l.plan_get(&key)?.filter(|p| p.rail == rail) else { return Ok(false) };
    plan.active = false;
    plan.paid_until_ms = plan.paid_until_ms.min(now_ms);
    plan.checked_at_ms = now_ms;
    close_period(l, &key, &mut plan)?;
    l.allowance_lapse(&key)?;
    if disputed {
        plan.disputed_at_ms = now_ms;
    }
    l.plan_put(&key, &plan)?;
    Ok(true)
}

/// Move an App Store plan to another account — only on an explicit "Restore purchases",
/// for someone who lost their phrase or set up a device apart. The period moves as it
/// stands, never a fresh one; never onto a plan the target is paying for.
pub fn move_plan(l: &Ledger, rail: &str, to_account: &str, now_ms: u64) -> Result<bool, String> {
    let to = l.acct_key(to_account);
    let Some(from) = l.rail_owner(rail)? else { return Ok(false) };
    if from == to {
        return Ok(false);
    }
    if l.plan_get(&to)?.is_some_and(|p| p.rail != rail && p.paid_at(now_ms)) {
        return Ok(false);
    }
    let Some(plan) = l.plan_get(&from)?.filter(|p| p.rail == rail) else { return Ok(false) };
    if let Some((period, ends, granted, left)) = l.allowance_of(&from)? {
        l.allowance_set(&to, period as u64 * 1000, ends, granted)?;
        let used = granted.saturating_sub(left);
        if used > 0 {
            // allowance_set sets left = granted; take the used part off again.
            l.allowance_take(&to, used)?;
        }
        l.allowance_lapse(&from)?;
    }
    l.plan_delete(&from)?;
    l.plan_put(&to, &plan)?;
    l.rail_bind(rail, &to)?;
    Ok(true)
}

/// Plans whose rail was not asked about for `stale_ms`, stalest first: (stored name, plan).
pub fn stale(l: &Ledger, now_ms: u64, stale_ms: u64) -> Result<Vec<(String, Plan)>, String> {
    let mut out: Vec<(String, Plan)> = Vec::new();
    for key in l.plan_keys()? {
        if let Some(p) = l.plan_get(&key)? {
            if now_ms.saturating_sub(p.checked_at_ms) >= stale_ms {
                out.push((key, p));
            }
        }
    }
    out.sort_by_key(|(_, p)| p.checked_at_ms);
    Ok(out)
}

/// The renewal check asked the rail and got no usable answer: do not ask again at once.
pub fn mark_checked(l: &Ledger, key: &str, now_ms: u64) -> Result<(), String> {
    if let Some(mut p) = l.plan_get(key)? {
        p.checked_at_ms = now_ms;
        l.plan_put(key, &p)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokumai_core::subscription::{add_months_ms, ms_from_civil};

    const DAY: u64 = 86_400_000;
    fn ms(y: i32, m: u32, d: u32) -> u64 {
        ms_from_civil(y, m, d)
    }
    fn ledger() -> Ledger {
        Ledger::in_memory([1; 32]).unwrap()
    }
    fn left(l: &Ledger, a: &str, now: u64) -> u64 {
        l.balance(a, now).unwrap().allowance
    }
    /// A monthly paid period starting `start`, as a rail reports it, seen at `now`.
    fn monthly(l: &Ledger, a: &str, tier: usize, rail: &str, start: u64, now: u64) -> Result<bool, String> {
        subscribe_or_renew(l, a, tier, false, rail, now, start, add_months_ms(start, 1))
    }

    #[test]
    fn a_plan_bought_mid_month_is_one_allowance_per_payment_on_both_rails() {
        for rail in ["stripe:sub_a", "iap:2000000111"] {
            let l = ledger();
            assert!(monthly(&l, "a", 0, rail, ms(2026, 9, 30), ms(2026, 9, 30)).unwrap());
            assert_eq!(left(&l, "a", ms(2026, 9, 30)), 700_000, "{rail}");
            assert_eq!(roll(&l, ms(2026, 10, 1)).unwrap(), 0, "{rail}: nothing happens on the 1st");
            assert!(!monthly(&l, "a", 0, rail, ms(2026, 9, 30), ms(2026, 10, 15)).unwrap(), "{rail}: the same period again");
            assert!(monthly(&l, "a", 0, rail, ms(2026, 10, 30), ms(2026, 10, 30)).unwrap(), "{rail}: the renewal");
        }
    }

    #[test]
    fn an_app_store_renewal_grants_once_per_payment() {
        let l = ledger();
        let mut grants = 0;
        let mut start = ms(2026, 9, 20);
        for _ in 0..4 {
            let next = add_months_ms(start, 1);
            let mut t = start;
            while t < next {
                roll(&l, t).unwrap();
                if monthly(&l, "a", 0, "iap:1", start, t).unwrap() {
                    grants += 1;
                }
                t += DAY;
            }
            start = next;
        }
        assert_eq!(grants, 4);
    }

    #[test]
    fn a_yearly_plan_is_twelve_monthly_allowances_behind_one_payment() {
        let l = ledger();
        assert!(subscribe_or_renew(&l, "a", 2, true, "iap:9", ms(2026, 9, 15), ms(2026, 9, 15), ms(2027, 9, 15)).unwrap());
        let mut granted = 1;
        let mut t = ms(2026, 9, 16);
        while t < ms(2027, 9, 15) {
            granted += roll(&l, t).unwrap();
            t += DAY;
        }
        assert_eq!(granted, 12);
        roll(&l, ms(2027, 9, 16)).unwrap();
        assert_eq!(left(&l, "a", ms(2027, 9, 16)), 0, "it stops with the year");
    }

    #[test]
    fn a_late_renewal_is_granted_once_and_an_ended_period_lapses() {
        let l = ledger();
        monthly(&l, "a", 0, "iap:1", ms(2026, 9, 20), ms(2026, 9, 20)).unwrap();
        roll(&l, ms(2026, 10, 21)).unwrap();
        assert_eq!(left(&l, "a", ms(2026, 10, 21)), 0);
        assert!(monthly(&l, "a", 0, "iap:1", ms(2026, 10, 20), ms(2026, 11, 5)).unwrap());
        assert!(!monthly(&l, "a", 0, "iap:1", ms(2026, 10, 20), ms(2026, 11, 6)).unwrap());
        assert_eq!(left(&l, "a", ms(2026, 11, 6)), 700_000);
    }

    #[test]
    fn a_second_plan_is_refused_and_a_replaced_rail_stays_with_its_account() {
        let l = ledger();
        monthly(&l, "a", 0, "stripe:sub_old", ms(2026, 9, 1), ms(2026, 9, 1)).unwrap();
        assert_eq!(monthly(&l, "a", 0, "iap:2", ms(2026, 9, 5), ms(2026, 9, 5)).unwrap_err(), PLAN_ALREADY);
        // The old plan lapses; a new one on the same account is fine.
        roll(&l, ms(2026, 10, 2)).unwrap();
        assert!(monthly(&l, "a", 0, "stripe:sub_new", ms(2026, 10, 5), ms(2026, 10, 5)).unwrap());
        // The old rail may not become a month on a fresh account.
        assert_eq!(monthly(&l, "fresh", 0, "stripe:sub_old", ms(2026, 10, 6), ms(2026, 10, 6)).unwrap_err(), PLAN_ELSEWHERE);
    }

    #[test]
    fn an_upgrade_adds_the_rest_of_the_period_and_a_downgrade_waits() {
        let l = ledger();
        monthly(&l, "a", 0, "stripe:sub_a", ms(2026, 9, 1), ms(2026, 9, 1)).unwrap();
        assert_eq!(change_tier(&l, "a", 1, ms(2026, 9, 19)).unwrap(), 320_000);
        assert_eq!(left(&l, "a", ms(2026, 9, 19)), 1_020_000);
        assert_eq!(change_tier(&l, "a", 0, ms(2026, 9, 20)).unwrap(), 0);
        assert!(monthly(&l, "a", 0, "stripe:sub_a", ms(2026, 10, 1), ms(2026, 10, 1)).unwrap());
        assert_eq!(left(&l, "a", ms(2026, 10, 1)), 700_000);
    }

    #[test]
    fn money_back_ends_the_plan_now_and_keeps_one_usage_step_per_period() {
        let l = ledger();
        subscribe_or_renew(&l, "a", 2, true, "stripe:sub_y", ms(2026, 6, 10), ms(2026, 6, 10), ms(2027, 6, 10)).unwrap();
        let mut l = l;
        for d in [ms(2026, 7, 10), ms(2026, 8, 10)] {
            let h = l.hold("a", left(&l, "a", d - DAY), d - DAY).unwrap();
            l.settle(h, u64::MAX).unwrap();
            roll(&l, d).unwrap();
        }
        let h = l.hold("a", 1_000_000, ms(2026, 8, 15)).unwrap();
        l.settle(h, 1_000_000).unwrap();
        assert!(revoke(&l, "stripe:sub_y", true, ms(2026, 8, 20)).unwrap());
        assert_eq!(left(&l, "a", ms(2026, 8, 20)), 0);
        for m in 9..=12 {
            roll(&l, ms(2026, m, 11)).unwrap();
            assert_eq!(left(&l, "a", ms(2026, m, 11)), 0, "month {m} does not arrive");
        }
        let steps: Vec<String> = plan_of(&l, "a").unwrap().unwrap().usage.iter().map(|u| u.drawn.clone()).collect();
        assert_eq!(steps, vec!["all", "all", "some"]);
        roll(&l, ms(2026, 12, 1)).unwrap();
        assert_eq!(plan_of(&l, "a").unwrap().unwrap().usage.len(), 3, "kept while the dispute runs");
        roll(&l, ms(2027, 3, 1)).unwrap();
        assert!(plan_of(&l, "a").unwrap().unwrap().usage.is_empty());
    }

    #[test]
    fn a_plan_moves_only_as_it_stands_and_never_over_a_paid_one() {
        let l = ledger();
        let now = ms(2026, 9, 10);
        monthly(&l, "lost", 1, "iap:5", ms(2026, 9, 1), now).unwrap();
        let mut l = l;
        let h = l.hold("lost", 900_000, now).unwrap();
        l.settle(h, 900_000).unwrap();
        monthly(&l, "busy", 0, "stripe:sub_b", ms(2026, 9, 1), now).unwrap();
        assert!(!move_plan(&l, "iap:5", "busy", now).unwrap(), "busy's own plan would be lost");
        assert!(move_plan(&l, "iap:5", "fresh", now).unwrap());
        assert_eq!(left(&l, "fresh", now), 600_000, "the period moved as it stood");
        assert_eq!(left(&l, "lost", now), 0);
        assert!(!monthly(&l, "fresh", 1, "iap:5", ms(2026, 9, 1), now + DAY).unwrap(), "no fresh month from the move");
    }

    #[test]
    fn the_rail_table_does_not_hold_payment_references() {
        let l = ledger();
        monthly(&l, "a", 0, "iap:2000000999", ms(2026, 9, 1), ms(2026, 9, 1)).unwrap();
        assert!(l.rail_owner("iap:2000000999").unwrap().is_some());
        // The lookup table stores a keyed hash (M13); a raw scan finds no transaction number.
        assert!(!l.debug_dump_rails().iter().any(|r| r.contains("2000000999")));
    }
}
