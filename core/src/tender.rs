// ---------------------------------------------------------------------------
// tender.rs — paying for ONE request with coins, without a session balance.
//
// The problem this solves: coins must be handed over BEFORE the server does the work,
// but what the work costs is only known AFTER it. A single ecash payment is atomic —
// submitting it burns all of its coins — so paying "the ceiling" with one payment would
// charge the worst case every time, and there is no way to give change that would not
// re-link the payer.
//
// The tender: the client attaches SEVERAL payments whose values are 1, 2, 4, 8, … so that
// every whole number of coins up to the ceiling is an exact subset sum. The server burns
// exactly the subset the answer cost and leaves the rest UNSUBMITTED — those payments are
// still good, so the client keeps them and tenders them again next time. Nothing is lost,
// no change has to be handed back, and the server learns only what this one request cost.
//
// Rounding is therefore the coin, not the ceiling: a request costs `ceil(price / coin)`.
// That is why the coin is 0.1 ¢ (docs/unlinkability.md, block D).
//
// Reuse of an unsubmitted payment is safe as long as the client re-sends it VERBATIM,
// with its original `pay_info`: a server that did burn it after all sees the identical
// (serials, pay_info) pair and answers `Verdict::Replay`, never a double-spend.
// ---------------------------------------------------------------------------
use serde::{Deserialize, Serialize};

use nym_compact_ecash::scheme::{PayInfo, Payment};

/// Most payments one tender may carry. A plan for `c` coins needs ⌈log2(c+1)⌉ + 1 of
/// them, so this covers a ceiling of ~32k coins ($32) — far past any single request.
pub const MAX_NOTES: usize = 16;

/// One payment in a tender: its face value in coins, the payment itself, and the context
/// it was signed for. `pay_info` travels as raw bytes because `PayInfo` is not serde.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Note {
    pub coins: u64,
    pub payment: Payment,
    pub pay_info: Vec<u8>,
    pub spend_date: u32,
    /// TOKU one coin of this note is worth. It names which issuing key the payment
    /// belongs to, nothing more: a note claiming the coarse denomination while carrying
    /// fine coins simply fails verification against the coarse authority's key.
    #[serde(default = "default_denom")]
    pub denom_toku: u64,
    /// The expiration date of the book this note came out of — which issuing EPOCH it
    /// belongs to. A server serves several at once (an old epoch stays verifiable until
    /// its own date while new books come from the newest), so without this the server
    /// would have to try every key it holds. Like the denomination it only selects a key:
    /// a note that names the wrong epoch fails verification. 0 = whatever the server
    /// issues today, for notes written before epochs rolled.
    #[serde(default)]
    pub exp_date: u32,
}

fn default_denom() -> u64 {
    crate::coconut::COIN_TOKU
}

impl Note {
    /// What this note is worth.
    pub fn value_toku(&self) -> u64 {
        self.coins.saturating_mul(self.denom_toku)
    }

    pub fn pay_info(&self) -> Result<PayInfo, String> {
        let bytes: [u8; 72] = self.pay_info.as_slice().try_into().map_err(|_| "bad pay_info length".to_string())?;
        Ok(PayInfo { pay_info_bytes: bytes })
    }

    /// The face value must match the coins actually inside the payment — otherwise a
    /// client could claim a 16-coin note while spending one coin.
    pub fn coins_match(&self) -> bool {
        self.payment.ss.len() as u64 == self.coins
    }
}

/// The notes a client attached to one request.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Tender {
    pub notes: Vec<Note>,
}

impl Tender {
    pub fn total_coins(&self) -> u64 {
        self.notes.iter().map(|n| n.coins).sum()
    }

    /// What the whole tender is worth. Notes of different denominations sit side by side —
    /// the coarse ones carry the bulk, the fine ones make the amount exact — so the value
    /// is the sum of the notes' values, never a coin count times one size.
    pub fn total_toku(&self) -> u64 {
        self.notes.iter().map(|n| n.value_toku()).sum()
    }

    /// Shape check before any crypto: bounded, non-empty, every note internally consistent.
    pub fn well_formed(&self) -> Result<(), String> {
        if self.notes.is_empty() {
            return Err("no coins attached".into());
        }
        if self.notes.len() > MAX_NOTES {
            return Err(format!("too many notes in one tender (max {MAX_NOTES})"));
        }
        for n in &self.notes {
            if n.coins == 0 {
                return Err("a note with no coins".into());
            }
            if !crate::coconut::DENOMS.contains(&n.denom_toku) {
                return Err(format!("a note of an unknown denomination ({} TOKU)", n.denom_toku));
            }
            if !n.coins_match() {
                return Err("a note's face value does not match its payment".into());
            }
            n.pay_info()?;
        }
        Ok(())
    }

    /// Indices of the notes to burn for `cost` TOKU: the cheapest subset worth at least
    /// that much. With a planned tender (coarse notes for the bulk, a fine 1,2,4,… plan
    /// for the remainder) the sum is EXACT; with an arbitrary set it can overshoot, and
    /// then the client overpaid by design — it chose the notes. `None` when the tender is
    /// worth less than the cost.
    pub fn select(&self, cost_toku: u64) -> Option<Vec<usize>> {
        select_from(&self.notes.iter().map(|n| n.value_toku()).collect::<Vec<_>>(), cost_toku)
    }
}

/// Face values for a ceiling of `coins`: 1, 2, 4, … and one remainder, so every whole
/// number from 0 to `coins` is an exact subset sum and nothing is wasted. Empty for 0.
pub fn plan_coins(coins: u64) -> Vec<u64> {
    let mut out = Vec::new();
    let mut left = coins;
    let mut v = 1u64;
    while left > 0 {
        let take = v.min(left);
        out.push(take);
        left -= take;
        v = v.saturating_mul(2);
        if out.len() == MAX_NOTES && left > 0 {
            // Cannot split any finer without exceeding the note limit — put the rest in
            // the last note. Exactness is lost above this ceiling, which `MAX_NOTES` is
            // chosen to keep out of reach for a single request.
            *out.last_mut().expect("just pushed") += left;
            break;
        }
    }
    out
}

/// Cheapest subset of `values` summing to at least `cost` (greedy from the largest —
/// exact for a `plan_coins` set). `None` if everything together is not enough.
pub fn select_from(values: &[u64], cost: u64) -> Option<Vec<usize>> {
    if cost == 0 {
        return Some(Vec::new());
    }
    if values.iter().sum::<u64>() < cost {
        return None;
    }
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by(|a, b| values[*b].cmp(&values[*a]));
    let mut left = cost;
    let mut picked = Vec::new();
    for i in order {
        if left == 0 {
            break;
        }
        if values[i] <= left {
            picked.push(i);
            left -= values[i];
        }
    }
    if left > 0 {
        // The remainder is smaller than every unpicked note: add the smallest one that
        // covers it (this is where a non-plan tender overshoots).
        let mut rest: Vec<usize> = (0..values.len()).filter(|i| !picked.contains(i)).collect();
        rest.sort_by(|a, b| values[*a].cmp(&values[*b]));
        let fill = rest.into_iter().find(|i| values[*i] >= left)?;
        picked.push(fill);
    }
    picked.sort_unstable();
    Some(picked)
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_can_pay_every_whole_number_of_coins_up_to_its_ceiling() {
        for ceiling in [1u64, 2, 3, 7, 8, 30, 31, 32, 100, 1000] {
            let values = plan_coins(ceiling);
            assert_eq!(values.iter().sum::<u64>(), ceiling, "plan for {ceiling} spends exactly the ceiling");
            assert!(values.len() <= MAX_NOTES, "plan for {ceiling} fits the note limit: {}", values.len());
            for cost in 0..=ceiling {
                let picked = select_from(&values, cost).unwrap_or_else(|| panic!("{ceiling}: no subset for {cost}"));
                let paid: u64 = picked.iter().map(|i| values[*i]).sum();
                assert_eq!(paid, cost, "ceiling {ceiling}: paying {cost} burned {paid}");
            }
        }
    }

    #[test]
    fn a_plan_stays_small() {
        // 1,2,4,…: a $1 ceiling at 0.1 ¢ per coin is 1000 coins and ten notes.
        assert_eq!(plan_coins(0), Vec::<u64>::new());
        assert_eq!(plan_coins(1), vec![1]);
        assert_eq!(plan_coins(10), vec![1, 2, 4, 3]);
        assert_eq!(plan_coins(1000).len(), 10);
        assert!(plan_coins(30_000).len() <= MAX_NOTES);
    }

    /// Coarse notes for the bulk, a fine plan for the remainder: every tenth of a cent up
    /// to the ceiling must still be payable EXACTLY, in a fraction of the coins. This is
    /// the whole point of the second denomination — a 9 ¢ ceiling used to be ninety coins
    /// on the table (44 KB); here it is nine coarse plus a fine plan.
    #[test]
    fn coarse_notes_carry_the_bulk_and_fine_ones_keep_it_exact() {
        use crate::coconut::{COARSE_TOKU, COIN_TOKU};
        let ceiling_toku = 9_000; // 9 ¢
        // What the client builds: whole coarse coins, then one fine book's plan.
        let coarse = ceiling_toku / COARSE_TOKU;
        let mut values: Vec<u64> = (0..coarse).map(|_| COARSE_TOKU).collect();
        values.extend(plan_coins(COARSE_TOKU / COIN_TOKU).iter().map(|c| c * COIN_TOKU));
        assert!(values.len() <= MAX_NOTES, "{} notes", values.len());
        assert_eq!(values.iter().filter(|v| **v == COARSE_TOKU).count(), 9);

        // Every tenth of a cent from nothing up to the ceiling is an exact subset sum.
        for cost in (0..=ceiling_toku).step_by(COIN_TOKU as usize) {
            let picked = select_from(&values, cost).unwrap_or_else(|| panic!("no subset for {cost}"));
            let paid: u64 = picked.iter().map(|i| values[*i]).sum();
            assert_eq!(paid, cost, "paying {cost} TOKU burned {paid}");
        }
        // And the coins actually on the table: 9 coarse + 10 fine = 19, not 90.
        let coins_on_table = coarse + (COARSE_TOKU / COIN_TOKU);
        assert_eq!(coins_on_table, 19);
    }

    #[test]
    fn a_tender_worth_less_than_the_cost_cannot_pay() {
        assert!(select_from(&[1, 2, 4], 8).is_none());
        assert_eq!(select_from(&[1, 2, 4], 0), Some(vec![]));
    }

    #[test]
    fn an_arbitrary_tender_overshoots_rather_than_underpays() {
        // Not a plan: 5 and 5 cannot make 3, so one whole note is burned.
        let picked = select_from(&[5, 5], 3).unwrap();
        assert_eq!(picked.len(), 1);
        assert_eq!(picked.iter().map(|i| [5u64, 5][*i]).sum::<u64>(), 5);
    }
}
