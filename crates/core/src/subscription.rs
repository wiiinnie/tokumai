// ---------------------------------------------------------------------------
// subscription.rs — the arithmetic of a monthly plan.
//
// Two jobs, both pure so they can be tested without a server, a clock or a card:
//
//   1. THE PERIOD. A plan runs in months counted from the day it was bought — the 15th
//      to the 15th — on BOTH rails, because that is how Apple bills and cannot be told
//      otherwise, and Stripe then bills the same way (decided 2026-09-21). One payment,
//      one allowance: no calendar month running beside the billing month, so no first
//      month granted twice (Apple) and no part-month to prorate (Stripe). A yearly plan
//      is twelve of these months behind one payment. `slice_at` is the whole rule.
//
//   2. THE TWO POCKETS. A subscriber's month lives in `Allowance` (set when a period
//      begins, lapses with it); everything bought as one-off credit stays in the
//      account's entitlement and is never touched by a reset. Spending takes the
//      perishable pocket first; a return puts value back where it came from.
//
// The rule that makes (2) safe is `give_back`: a device handing coins back must not
// be able to turn a monthly allowance into permanent credit. Draw a million when the
// period begins, hand it back the day before it ends, repeat — and a year later the account would hold a
// year of "credit that never expires", which is the forfeiture problem the whole
// subscription exists to avoid. See docs/subscription.md.
// ---------------------------------------------------------------------------

use serde::{Deserialize, Serialize};

/// The three plans as (TOKU a month, euro cents a month). Decided 2026-09-18; the
/// reasoning, including why the allowance rather than `MARGIN` carries the margin, is in
/// docs/subscription.md. The rate improves along the ladder — 70,000 / 75,000 / 80,000
/// TOKU per euro — so the saving shown on a tier is a real and growing number.
/// The yearly price is twelve months less 10 %, computed by `yearly_cents`.
pub const TIERS: [(u64, u64); 3] = [(700_000, 1000), (1_500_000, 2000), (4_000_000, 5000)];

/// What a tier's allowance would cost at the ENTRY tier's rate, in cents — the number the
/// subscribe sheet turns into "saves €1.42 a month". Derived, never typed: a hand-written
/// saving is a promise that goes stale the first time a tier moves.
///
/// The comparison price is FLOORED, so the saving is understated by at most a cent rather
/// than overstated by one. A number that promises a benefit rounds against us.
pub fn saving_cents(tier: usize) -> u64 {
    let (entry_toku, entry_cents) = TIERS[0];
    let Some(&(toku, cents)) = TIERS.get(tier) else { return 0 };
    let at_entry_rate = toku * entry_cents / entry_toku;
    at_entry_rate.saturating_sub(cents)
}

/// Yearly = twelve months less 10 %, rounded to the cent (down — in the customer's favour).
pub fn yearly_cents(monthly_cents: u64) -> u64 {
    (monthly_cents * 12 * 90) / 100
}

/// Calendar days in a month. 1-based month; a month outside 1..=12 is treated as 31 so
/// a corrupted date can never produce a zero divisor.
pub fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 31,
    }
}

/// What an upgrade adds for the rest of a period: the step between two tiers, scaled by
/// the part of the period still to run, floored — the fraction of a TOKU goes to nobody's
/// side but ours, and the customer gets every whole one. Time-based (milliseconds), which
/// is also how Stripe prorates an upgrade, so the two cannot drift apart.
pub fn prorata_toku(step_toku: u64, remaining_ms: u64, total_ms: u64) -> u64 {
    if total_ms == 0 {
        return 0;
    }
    (step_toku as u128 * remaining_ms.min(total_ms) as u128 / total_ms as u128) as u64
}

/// UTC (year, month, day) from a unix-millisecond timestamp — Howard Hinnant's
/// `civil_from_days`, which is exact for any date we will ever see and avoids pulling a
/// date library into the shared core. Everything a subscription needs is UTC.
pub fn civil_from_ms(ms: u64) -> (i32, u32, u32) {
    let days = (ms / 86_400_000) as i64 + 719_468; // shift epoch to 0000-03-01
    let era = days.div_euclid(146_097);
    let doe = days.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((y + i64::from(m <= 2)) as i32, m, d)
}

/// Unix-millisecond timestamp of a UTC date at midnight — the inverse of `civil_from_ms`
/// (Hinnant's `days_from_civil`).
pub fn ms_from_civil(y: i32, m: u32, d: u32) -> u64 {
    let (yy, mm) = if m <= 2 { (y as i64 - 1, m + 9) } else { (y as i64, m - 3) };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * mm as i64 + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    ((era * 146_097 + doe - 719_468).max(0) * 86_400_000) as u64
}

/// `ms` moved on by `months` calendar months, at the same time of day. A day the target
/// month does not have is clamped to its last day — the 31st of January plus one month is
/// the 28th (or 29th) of February — which is what Apple does with a renewal date too.
pub fn add_months_ms(ms: u64, months: u32) -> u64 {
    let (y, m, d) = civil_from_ms(ms);
    let idx = (y as i64) * 12 + (m as i64 - 1) + months as i64;
    let (ty, tm) = (idx.div_euclid(12) as i32, (idx.rem_euclid(12) + 1) as u32);
    let td = d.min(days_in_month(ty, tm));
    ms_from_civil(ty, tm, td) + ms % 86_400_000
}

/// The allowance period a subscription is in at `now`, as (start, end) in unix ms — or
/// `None` when the paid period is over (or not known: `paid_until_ms` 0 fails closed).
///
/// `period_start_ms` / `paid_until_ms` are what the RAIL says the current paid period is:
/// Apple's `purchaseDate`/`expiresDate` of the latest transaction, Stripe's
/// `current_period_start`/`_end`. A MONTHLY plan's allowance period is exactly that paid
/// period — one payment, one grant, and a renewal is simply the next period starting. A
/// YEARLY plan's paid period is a year and is cut into twelve months from its start; the
/// last may end early if the rail says the year does.
///
/// The start is what identifies a period (`Allowance::period`), so "has a new one begun"
/// is one compare, and a renewal reported late — the app opened days after Apple charged —
/// still finds its period, once.
pub fn slice_at(period_start_ms: u64, paid_until_ms: u64, yearly: bool, now_ms: u64) -> Option<(u64, u64)> {
    if paid_until_ms <= now_ms || period_start_ms == 0 || period_start_ms >= paid_until_ms {
        return None;
    }
    if !yearly {
        return Some((period_start_ms, paid_until_ms));
    }
    // Whole months from the start of the paid year to now. A clock a little behind the
    // rail's (now < start) is month 0, never a negative month.
    let (y0, m0, _) = civil_from_ms(period_start_ms);
    let (y1, m1, _) = civil_from_ms(now_ms.max(period_start_ms));
    let mut k = ((y1 as i64 - y0 as i64) * 12 + m1 as i64 - m0 as i64).clamp(0, 11) as u32;
    while k > 0 && add_months_ms(period_start_ms, k) > now_ms {
        k -= 1;
    }
    let start = add_months_ms(period_start_ms, k);
    let end = if k >= 11 { paid_until_ms } else { add_months_ms(period_start_ms, k + 1).min(paid_until_ms) };
    Some((start, end))
}

/// The subscriber's month.
///
/// Invariant while a period is open: `left + outstanding_of_this_period == granted`.
/// `outstanding` counts across periods on purpose — it is what stops coins drawn from
/// LAST month's allowance from arriving as permanent credit this month.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Allowance {
    /// Which period this allowance was granted for: its START, in unix SECONDS (see
    /// `slice_at`). 0 = none yet. Until 2026-09-21 this was a calendar `YYYYMM`; such a
    /// value reads as a moment in January 1970, so a row that old simply counts as an
    /// earlier period and the next one is granted normally.
    pub period: u32,
    /// When that period ends, in unix ms — the date the app shows as "renews" and the
    /// moment an allowance whose plan did not renew lapses. 0 on a row from before.
    #[serde(default)]
    pub ends_ms: u64,
    /// What the period was granted (more in a month that was upgraded).
    pub granted: u64,
    /// What is still on the account, waiting to be drawn onto a device.
    pub left: u64,
    /// Allowance-derived value drawn onto devices and not yet handed back. The ceiling
    /// for how much of a return may go back into an allowance rather than to entitlement.
    pub outstanding: u64,
}

impl Allowance {
    /// A new period begins: **set**, never add. An `+=` here would let unspent allowance
    /// accumulate, and an accumulating balance is the voucher the subscription replaced.
    /// `outstanding` survives — coins from the old period are still out there.
    pub fn reset(&mut self, start_ms: u64, end_ms: u64, granted: u64) {
        self.period = Self::period_key(start_ms);
        self.ends_ms = end_ms;
        self.granted = granted;
        self.left = granted;
    }

    /// The `period` value for a period starting at `start_ms`.
    pub fn period_key(start_ms: u64) -> u32 {
        (start_ms / 1000).min(u32::MAX as u64) as u32
    }

    /// An upgrade mid-period: more allowance for the rest of the period, added to both the
    /// grant and what is left (the customer keeps what they had).
    pub fn add_upgrade(&mut self, extra: u64) {
        self.granted = self.granted.saturating_add(extra);
        self.left = self.left.saturating_add(extra);
    }

    /// The period is over and nothing renewed it: nothing left to draw. `outstanding`
    /// stays — see `give_back`.
    pub fn lapse(&mut self) {
        self.granted = 0;
        self.left = 0;
    }

    /// Take `want` TOKU, allowance first. Returns (from the allowance, still to be found
    /// in entitlement). The perishable pocket goes first because it dies with its period
    /// either way and bought credit does not — the order that costs the customer least.
    pub fn spend(&mut self, want: u64) -> (u64, u64) {
        let from_allowance = want.min(self.left);
        self.left -= from_allowance;
        self.outstanding = self.outstanding.saturating_add(from_allowance);
        (from_allowance, want - from_allowance)
    }

    /// Coins handed back from a device. Returns (restored to the allowance, credited as
    /// entitlement) — and what those two numbers do NOT contain is the point:
    ///
    /// * up to `outstanding`, the value goes back to the allowance it came from, capped at
    ///   `granted`. Anything over that cap is **destroyed**: it is last month's allowance,
    ///   handed back after the month ended, and it must not reappear as anything;
    /// * only what exceeds `outstanding` — value that was bought, not granted — becomes
    ///   entitlement, which never expires.
    ///
    /// So an orderly device change costs a subscriber nothing, and no amount of
    /// draw-and-return turns a monthly allowance into a permanent balance.
    pub fn give_back(&mut self, value: u64) -> (u64, u64) {
        let from_allowance = value.min(self.outstanding);
        self.outstanding -= from_allowance;
        let room = self.granted.saturating_sub(self.left);
        let restored = from_allowance.min(room);
        self.left += restored;
        (restored, value - from_allowance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_calendar_comes_out_of_a_timestamp_without_a_date_library() {
        // Known instants, checked against `date -u -r <s>`.
        assert_eq!(civil_from_ms(0), (1970, 1, 1));
        assert_eq!(civil_from_ms(1_789_000_000_000), (2026, 9, 10));
        assert_eq!(civil_from_ms(1_756_684_800_000), (2025, 9, 1));
        // Leap day, and the day after.
        assert_eq!(civil_from_ms(1_709_164_800_000), (2024, 2, 29));
        assert_eq!(civil_from_ms(1_709_251_200_000), (2024, 3, 1));
        // …and back again.
        assert_eq!(ms_from_civil(2024, 2, 29), 1_709_164_800_000);
        assert_eq!(ms_from_civil(1970, 1, 1), 0);
    }

    #[test]
    fn a_month_on_is_the_same_day_or_the_last_one_the_month_has() {
        let noon = 12 * 3_600_000;
        let d = |y, m, dd| ms_from_civil(y, m, dd) + noon;
        assert_eq!(add_months_ms(d(2026, 9, 15), 1), d(2026, 10, 15), "and the time of day stays");
        assert_eq!(add_months_ms(d(2026, 12, 15), 1), d(2027, 1, 15));
        assert_eq!(add_months_ms(d(2026, 1, 31), 1), d(2026, 2, 28), "clamped, like Apple");
        assert_eq!(add_months_ms(d(2028, 1, 31), 1), d(2028, 2, 29));
        assert_eq!(add_months_ms(d(2026, 1, 31), 2), d(2026, 3, 31), "counted from the START, not chained");
        assert_eq!(add_months_ms(d(2026, 9, 15), 12), d(2027, 9, 15));
    }

    #[test]
    fn a_monthly_plan_is_one_period_per_payment() {
        let (s, e) = (ms_from_civil(2026, 9, 15), ms_from_civil(2026, 10, 15));
        assert_eq!(slice_at(s, e, false, s), Some((s, e)));
        assert_eq!(slice_at(s, e, false, e - 1), Some((s, e)));
        assert_eq!(slice_at(s, e, false, e), None, "the paid period is over");
        assert_eq!(slice_at(0, e, false, s), None, "no known start fails closed");
        assert_eq!(slice_at(s, 0, false, s), None, "no known end fails closed");
    }

    #[test]
    fn a_yearly_plan_is_twelve_months_from_its_start() {
        let s = ms_from_civil(2026, 1, 31);
        let e = ms_from_civil(2027, 1, 31);
        assert_eq!(slice_at(s, e, true, s), Some((s, ms_from_civil(2026, 2, 28))));
        assert_eq!(
            slice_at(s, e, true, ms_from_civil(2026, 3, 1)),
            Some((ms_from_civil(2026, 2, 28), ms_from_civil(2026, 3, 31)))
        );
        // The last month ends with the year.
        assert_eq!(
            slice_at(s, e, true, ms_from_civil(2027, 1, 30)),
            Some((ms_from_civil(2026, 12, 31), e))
        );
        assert_eq!(slice_at(s, e, true, e), None);
        // Exactly twelve distinct periods across the year.
        let mut starts = std::collections::BTreeSet::new();
        let mut t = s;
        while let Some((a, _)) = slice_at(s, e, true, t) {
            starts.insert(a);
            t += 86_400_000;
        }
        assert_eq!(starts.len(), 12);
    }

    #[test]
    fn an_upgrade_is_prorated_by_the_time_left_in_the_period() {
        let day = 86_400_000u64;
        assert_eq!(prorata_toku(800_000, 12 * day, 30 * day), 320_000);
        assert_eq!(prorata_toku(800_000, 0, 30 * day), 0);
        assert_eq!(prorata_toku(800_000, 40 * day, 30 * day), 800_000, "never more than the step");
        assert_eq!(prorata_toku(800_000, day, 0), 0, "no zero divisor");
    }

    /// Midnight UTC on the 1st of a month, for tests that only need "some period".
    fn mo(y: i32, m: u32) -> u64 {
        ms_from_civil(y, m, 1)
    }

    #[test]
    fn yearly_is_twelve_months_less_ten_percent() {
        assert_eq!(yearly_cents(1000), 10_800);
        assert_eq!(yearly_cents(2000), 21_600);
        assert_eq!(yearly_cents(5000), 54_000);
    }

    #[test]
    fn a_bigger_plan_saves_a_real_and_growing_amount() {
        // 1.5M TOKU at the entry rate is EUR 21.42 floored, so EUR 20 saves EUR 1.42;
        // 4M is EUR 57.14, so EUR 50 saves EUR 7.14.
        assert_eq!(saving_cents(0), 0);
        assert_eq!(saving_cents(1), 142);
        assert_eq!(saving_cents(2), 714);
        // It must GROW along the ladder — two tiers showing the same saving reads as a bug
        // and invites "why not just buy the middle one twice".
        assert!(saving_cents(2) > saving_cents(1));
        assert_eq!(saving_cents(9), 0); // out of range is 0, never a panic
    }

    #[test]
    fn the_month_resets_to_the_grant_and_never_accumulates() {
        let mut a = Allowance::default();
        a.reset(mo(2026, 9), add_months_ms(mo(2026, 9), 1), 1_000_000);
        a.spend(300_000);
        assert_eq!(a.left, 700_000);
        // Unspent allowance does NOT roll into October.
        a.reset(mo(2026, 10), add_months_ms(mo(2026, 10), 1), 1_000_000);
        assert_eq!(a.left, 1_000_000);
        assert_eq!(a.granted, 1_000_000);
    }

    #[test]
    fn spending_empties_the_perishable_pocket_first() {
        let mut a = Allowance::default();
        a.reset(mo(2026, 9), add_months_ms(mo(2026, 9), 1), 1_000_000);
        // Well inside the allowance: nothing is asked of entitlement.
        assert_eq!(a.spend(400_000), (400_000, 0));
        // Past it: the remainder is handed to the caller to take from entitlement.
        assert_eq!(a.spend(900_000), (600_000, 300_000));
        assert_eq!(a.left, 0);
        assert_eq!(a.outstanding, 1_000_000);
    }

    #[test]
    fn a_device_change_mid_month_costs_a_subscriber_nothing() {
        let mut a = Allowance::default();
        a.reset(mo(2026, 9), add_months_ms(mo(2026, 9), 1), 1_000_000);
        a.spend(1_000_000); // the whole month drawn onto the old phone
        // 800k of it unspent, handed back when the phone is retired.
        assert_eq!(a.give_back(800_000), (800_000, 0));
        assert_eq!(a.left, 800_000);
        assert_eq!(a.outstanding, 200_000);
        // …and it can be drawn again onto the new phone.
        assert_eq!(a.spend(800_000), (800_000, 0));
    }

    #[test]
    fn draw_and_return_can_never_build_a_permanent_balance() {
        let mut a = Allowance::default();
        let mut entitlement = 0u64;
        for m in 9..=12 {
            a.reset(mo(2026, m), add_months_ms(mo(2026, m), 1), 1_000_000);
            a.spend(1_000_000); // draw the month
            let (_, to_entitlement) = a.give_back(900_000); // hand most of it back
            entitlement += to_entitlement;
        }
        // Four months of draw-and-return, and not one TOKU became permanent credit.
        assert_eq!(entitlement, 0);
        // Nor is more ever spendable in a month than the month granted.
        assert!(a.left <= a.granted);
    }

    #[test]
    fn last_months_coins_handed_back_this_month_are_not_credit() {
        let mut a = Allowance::default();
        a.reset(mo(2026, 9), add_months_ms(mo(2026, 9), 1), 1_000_000);
        a.spend(1_000_000);
        // October: a fresh, untouched allowance.
        a.reset(mo(2026, 10), add_months_ms(mo(2026, 10), 1), 1_000_000);
        // September's coins arrive back. October is already full, so there is no room —
        // the value is destroyed rather than credited. It was a month that has ended.
        assert_eq!(a.give_back(1_000_000), (0, 0));
        assert_eq!(a.left, 1_000_000);
        assert_eq!(a.outstanding, 0);
    }

    #[test]
    fn coins_bought_with_credit_come_back_as_credit() {
        let mut a = Allowance::default();
        a.reset(mo(2026, 9), add_months_ms(mo(2026, 9), 1), 1_000_000);
        // A big withdrawal: the allowance covers part, entitlement the rest.
        let (from_allowance, from_entitlement) = a.spend(1_500_000);
        assert_eq!((from_allowance, from_entitlement), (1_000_000, 500_000));
        // All of it handed back: the granted part refills the month, the BOUGHT part
        // returns as entitlement, which never expires.
        assert_eq!(a.give_back(1_500_000), (1_000_000, 500_000));
    }

    #[test]
    fn an_account_with_no_subscription_hands_everything_back_as_credit() {
        // The customer who bought one-off credit and never subscribes: no allowance is
        // involved, so the return path behaves exactly as it does today.
        let mut a = Allowance::default();
        assert_eq!(a.spend(900_000), (0, 900_000));
        assert_eq!(a.give_back(900_000), (0, 900_000));
    }

    #[test]
    fn an_upgrade_adds_to_the_month_without_resetting_it() {
        let mut a = Allowance::default();
        a.reset(mo(2026, 9), add_months_ms(mo(2026, 9), 1), 1_000_000);
        a.spend(700_000);
        // 12 of 30 days of the period left: 40 % of the +1M step.
        let day = 86_400_000;
        let extra = prorata_toku(1_000_000, 12 * day, 30 * day);
        assert_eq!(extra, 400_000);
        a.add_upgrade(extra);
        assert_eq!(a.left, 700_000); // 300k of the old month + 400k added
        assert_eq!(a.granted, 1_400_000);
    }

    #[test]
    fn a_lapsed_subscription_leaves_nothing_to_draw() {
        let mut a = Allowance::default();
        a.reset(mo(2026, 9), add_months_ms(mo(2026, 9), 1), 1_000_000);
        a.spend(400_000);
        a.lapse();
        assert_eq!(a.spend(100_000), (0, 100_000)); // entitlement only, if any
        // The 400k already on a device is still theirs, and still returnable.
        assert_eq!(a.give_back(400_000), (0, 0));
    }
}
