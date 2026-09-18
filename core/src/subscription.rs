// ---------------------------------------------------------------------------
// subscription.rs — the arithmetic of a monthly plan.
//
// Two jobs, both pure so they can be tested without a server, a clock or a card:
//
//   1. PRO RATA. A month is 28 to 31 days, so a part-month is computed against the
//      REAL month, never against a nominal 30. Price and allowance come from ONE
//      fraction here, which is the only way they cannot drift apart.
//
//   2. THE TWO POCKETS. A subscriber's month lives in `Allowance` (set on the 1st,
//      lapses with the period); everything bought as one-off credit stays in the
//      account's entitlement and is never touched by a reset. Spending takes the
//      perishable pocket first; a return puts value back where it came from.
//
// The rule that makes (2) safe is `give_back`: a device handing coins back must not
// be able to turn a monthly allowance into permanent credit. Draw a million on the
// 1st, hand it back on the 30th, repeat — and a year later the account would hold a
// year of "credit that never expires", which is the forfeiture problem the whole
// subscription exists to avoid. See docs/subscription.md.
// ---------------------------------------------------------------------------

use serde::{Deserialize, Serialize};

/// The three plans. TOKU per month and the price in euro cents, monthly.
/// The yearly price is twelve months less 10 %, computed by `yearly_cents`.
pub const TIERS: [(u64, u64); 3] = [(1_000_000, 1000), (2_000_000, 2000), (3_000_000, 3000)];

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

/// The fraction of a month served from `start_day` to its end, as (served, total) days.
/// The start day COUNTS — service begins that day, so 15 March serves 17 of 31 days.
pub fn served(year: i32, month: u32, start_day: u32) -> (u32, u32) {
    let total = days_in_month(year, month);
    let start = start_day.clamp(1, total);
    (total - start + 1, total)
}

/// What a part-month costs: the full price scaled by the served fraction, rounded to the
/// nearest cent. Rounding the PRICE to nearest and the ALLOWANCE down is deliberate — the
/// cent goes wherever it falls, the fraction of a TOKU always to the customer's side.
pub fn prorata_cents(full_cents: u64, served_days: u32, total_days: u32) -> u64 {
    if total_days == 0 {
        return 0;
    }
    (full_cents * served_days as u64 * 2 + total_days as u64) / (total_days as u64 * 2)
}

/// What a part-month grants, floored.
pub fn prorata_toku(full_toku: u64, served_days: u32, total_days: u32) -> u64 {
    if total_days == 0 {
        return 0;
    }
    full_toku * served_days as u64 / total_days as u64
}

/// A calendar month as `YYYYMM` — the period an allowance belongs to. Comparable, so
/// "has the month rolled" is one integer compare and no date library.
pub fn period_of(year: i32, month: u32) -> u32 {
    (year.max(0) as u32) * 100 + month.clamp(1, 12)
}

/// The subscriber's month.
///
/// Invariant while a period is open: `left + outstanding_of_this_period == granted`.
/// `outstanding` counts across periods on purpose — it is what stops coins drawn from
/// LAST month's allowance from arriving as permanent credit this month.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Allowance {
    /// `YYYYMM` this allowance was granted for. 0 = none yet.
    pub period: u32,
    /// What the period was granted (pro rata in a first or upgraded month).
    pub granted: u64,
    /// What is still on the account, waiting to be drawn onto a device.
    pub left: u64,
    /// Allowance-derived value drawn onto devices and not yet handed back. The ceiling
    /// for how much of a return may go back into an allowance rather than to entitlement.
    pub outstanding: u64,
}

impl Allowance {
    /// The monthly reset: **set**, never add. An `+=` here would let unspent allowance
    /// accumulate, and an accumulating balance is the voucher the subscription replaced.
    /// `outstanding` survives — coins from the old month are still out there.
    pub fn reset(&mut self, period: u32, granted: u64) {
        self.period = period;
        self.granted = granted;
        self.left = granted;
    }

    /// An upgrade mid-month: more allowance for the rest of the period, added to both the
    /// grant and what is left (the customer keeps what they had).
    pub fn add_upgrade(&mut self, extra: u64) {
        self.granted = self.granted.saturating_add(extra);
        self.left = self.left.saturating_add(extra);
    }

    /// The month is over (or the subscription lapsed): nothing left to draw. `outstanding`
    /// stays — see `give_back`.
    pub fn lapse(&mut self) {
        self.granted = 0;
        self.left = 0;
    }

    /// Take `want` TOKU, allowance first. Returns (from the allowance, still to be found
    /// in entitlement). The perishable pocket goes first because it dies on the 1st either
    /// way and bought credit does not — the order that costs the customer least.
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
    fn a_part_month_is_computed_against_the_real_month() {
        // 15 March: 17 of 31 days → €5.48 and 548,387 TOKU (docs/subscription.md).
        let (s, t) = served(2026, 3, 15);
        assert_eq!((s, t), (17, 31));
        assert_eq!(prorata_cents(1000, s, t), 548);
        assert_eq!(prorata_toku(1_000_000, s, t), 548_387);

        // The SAME day in a different month is a different fraction — which is the whole
        // reason this is computed and not approximated at 30 days.
        let (s, t) = served(2026, 2, 15);
        assert_eq!((s, t), (14, 28));
        assert_eq!(prorata_cents(1000, s, t), 500);
        assert_eq!(prorata_toku(1_000_000, s, t), 500_000);

        // Leap February, and the first of a month is a whole month.
        assert_eq!(served(2028, 2, 15), (15, 29));
        assert_eq!(served(2026, 9, 1), (30, 30));
        assert_eq!(prorata_cents(1000, 30, 30), 1000);
    }

    #[test]
    fn the_last_day_of_a_month_still_serves_one_day() {
        let (s, t) = served(2026, 9, 30);
        assert_eq!((s, t), (1, 30));
        assert_eq!(prorata_cents(1000, s, t), 33);
        assert_eq!(prorata_toku(1_000_000, s, t), 33_333);
        // A day past the end cannot make the fraction negative or zero-divide.
        assert_eq!(served(2026, 9, 99), (1, 30));
    }

    #[test]
    fn yearly_is_twelve_months_less_ten_percent() {
        assert_eq!(yearly_cents(1000), 10_800);
        assert_eq!(yearly_cents(2000), 21_600);
        assert_eq!(yearly_cents(3000), 32_400);
    }

    #[test]
    fn the_month_resets_to_the_grant_and_never_accumulates() {
        let mut a = Allowance::default();
        a.reset(period_of(2026, 9), 1_000_000);
        a.spend(300_000);
        assert_eq!(a.left, 700_000);
        // Unspent allowance does NOT roll into October.
        a.reset(period_of(2026, 10), 1_000_000);
        assert_eq!(a.left, 1_000_000);
        assert_eq!(a.granted, 1_000_000);
    }

    #[test]
    fn spending_empties_the_perishable_pocket_first() {
        let mut a = Allowance::default();
        a.reset(period_of(2026, 9), 1_000_000);
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
        a.reset(period_of(2026, 9), 1_000_000);
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
            a.reset(period_of(2026, m), 1_000_000);
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
        a.reset(period_of(2026, 9), 1_000_000);
        a.spend(1_000_000);
        // October: a fresh, untouched allowance.
        a.reset(period_of(2026, 10), 1_000_000);
        // September's coins arrive back. October is already full, so there is no room —
        // the value is destroyed rather than credited. It was a month that has ended.
        assert_eq!(a.give_back(1_000_000), (0, 0));
        assert_eq!(a.left, 1_000_000);
        assert_eq!(a.outstanding, 0);
    }

    #[test]
    fn coins_bought_with_credit_come_back_as_credit() {
        let mut a = Allowance::default();
        a.reset(period_of(2026, 9), 1_000_000);
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
        a.reset(period_of(2026, 9), 1_000_000);
        a.spend(700_000);
        // 19 September: 12 of 30 days of the +1M step.
        let (s, t) = served(2026, 9, 19);
        let extra = prorata_toku(1_000_000, s, t);
        assert_eq!(extra, 400_000);
        assert_eq!(prorata_cents(1000, s, t), 400);
        a.add_upgrade(extra);
        assert_eq!(a.left, 700_000); // 300k of the old month + 400k added
        assert_eq!(a.granted, 1_400_000);
    }

    #[test]
    fn a_lapsed_subscription_leaves_nothing_to_draw() {
        let mut a = Allowance::default();
        a.reset(period_of(2026, 9), 1_000_000);
        a.spend(400_000);
        a.lapse();
        assert_eq!(a.spend(100_000), (0, 100_000)); // entitlement only, if any
        // The 400k already on a device is still theirs, and still returnable.
        assert_eq!(a.give_back(400_000), (0, 0));
    }
}
