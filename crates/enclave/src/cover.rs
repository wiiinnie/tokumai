//! Cover for the first question after a purchase.
//!
//! We are the merchant: we know who paid and when. Our host sees when a call goes out to a
//! model provider, and how big the answer was. While the service is small those two facts
//! join up — the first call after a payment is the payer's — and we would learn that a
//! named person asked something at a given moment, and whether it was a picture.
//!
//! Cryptography cannot touch this: even perfect blind credentials leave a payment recorded
//! at one minute and bytes leaving the machine two minutes later. Only traffic helps.
//!
//! So: from the moment a plan is paid for, the enclave counts what OTHER accounts are
//! doing. Every unrelated call in between breaks the "first call after the payment" tell.
//! When the new person finally asks something, the enclave counts the cover of the same
//! shape that piled up — a text call does not hide a picture — and buys as many decoys as
//! it takes to make up the difference, each offset the way ordinary traffic is offset.
//! Nobody's question is ever held back, so no one waits for this.
//!
//! The cost falls to zero as the service fills up, which is the point: decoys are bought
//! only while there is nobody to hide among. `tokumai-cover-need` computes the rest: with
//! 15 questions per user per day the text case pays for itself at about 250 users and the
//! picture case at about 2,500, and the bill peaks under a dollar a month on the way.
//!
//! Every payment arms this, renewals included, and that is deliberate. A renewal is
//! usually no anchor at all — the account has been making calls for weeks, so the payment
//! says nothing new about any one of them, and the count comes back covered and buys
//! nothing. The exception is an account that lies dormant and wakes after its renewal: its
//! first call of the month does follow its payment, exactly like a first purchase. Arming
//! on every payment catches that case and costs nothing in the ordinary one, which is why
//! a shared billing boundary — all renewals on one date — was considered and dropped
//! (docs/enclave-phase0.md, "Before launch").
//!
//! What it does not do: with a single active account there is nobody to hide among, so the
//! whole set is bought from us — and decoys we buy are, to an adversary who can repeat the
//! observation for months, our decoys. It blunts the sharpest edge; it does not make a
//! first purchase unlinkable.

use std::collections::HashMap;
use std::sync::Mutex;

/// What a call looks like from outside: a few kilobytes back, or several megabytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Text,
    Picture,
}

/// How many calls of the same shape, since the payment, count as enough cover — and, when
/// there are fewer, how many decoys are bought to make up the difference.
///
/// Five, because the measurement said the money does not matter (`tokumai-cover-need`): the
/// decoy bill peaks under a dollar a month at any number of users and falls to nothing as
/// the service fills up. A threshold chosen to save money would have been chosen for the
/// wrong reason; this one is chosen so that a first question stands among six calls rather
/// than two.
const ENOUGH: usize = 5;

/// How long a call counts as cover. Longer than any plausible gap between paying and
/// asking the first question; a payment older than this has stopped being an anchor.
const COVER_KEEP_MS: u64 = 6 * 3_600_000;

/// The most calls we remember. A busy enclave needs no more than this to answer "was
/// there cover", and the list is only ever read backwards.
const REMEMBER: usize = 512;

#[derive(Default)]
pub struct Cover {
    /// When each recent provider call happened, and what it looked like.
    seen: Mutex<Vec<(u64, Shape)>>,
    /// Accounts that have paid and not yet asked anything: when they paid.
    waiting: Mutex<HashMap<String, u64>>,
}

impl Cover {
    /// A plan was granted, or credit was added: this account's first question from here on
    /// is the one that could be tied to the payment.
    pub fn paid(&self, account: &str, now_ms: u64) {
        if let Ok(mut waiting) = self.waiting.lock() {
            waiting.retain(|_, at| now_ms.saturating_sub(*at) < COVER_KEEP_MS);
            waiting.insert(account.to_string(), now_ms);
        }
    }

    /// A call went out to a provider. Everyone's calls are cover for everyone else.
    pub fn note(&self, shape: Shape, now_ms: u64) {
        if let Ok(mut seen) = self.seen.lock() {
            seen.push((now_ms, shape));
            if seen.len() > REMEMBER {
                let cut = seen.len() - REMEMBER;
                seen.drain(..cut);
            }
        }
    }

    /// How many decoys this account's call needs beside it: enough to bring the calls of
    /// its shape up to [`ENOUGH`], counting what other people already provided. Asked once
    /// per account — after the first question the payment has stopped being an anchor,
    /// whether it was covered by other people's traffic or by our own money.
    pub fn decoys_needed(&self, account: &str, shape: Shape, now_ms: u64) -> usize {
        let Ok(mut waiting) = self.waiting.lock() else { return 0 };
        let Some(paid_at) = waiting.remove(account) else { return 0 };
        if now_ms.saturating_sub(paid_at) >= COVER_KEEP_MS {
            return 0; // long enough ago that the payment no longer points at anything
        }
        let cover = self
            .seen
            .lock()
            .map(|seen| seen.iter().filter(|(at, s)| *s == shape && *at >= paid_at).count())
            .unwrap_or(0);
        ENOUGH.saturating_sub(cover)
    }

    /// How many accounts are waiting for their first question (for the operator's view).
    pub fn waiting_count(&self) -> usize {
        self.waiting.lock().map(|w| w.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60_000;

    #[test]
    fn a_first_question_in_an_empty_room_is_covered_the_whole_way() {
        let cover = Cover::default();
        cover.paid("acct", 0);
        // Nothing else happened in between: we buy the whole set.
        assert_eq!(cover.decoys_needed("acct", Shape::Text, 2 * MIN), ENOUGH);
        // Asked once only: the payment has stopped being an anchor.
        assert_eq!(cover.decoys_needed("acct", Shape::Text, 3 * MIN), 0);
    }

    #[test]
    fn other_peoples_traffic_is_counted_off_what_we_have_to_buy() {
        let cover = Cover::default();
        cover.paid("acct", 0);
        for i in 1..=3 {
            cover.note(Shape::Text, i * MIN);
        }
        assert_eq!(cover.decoys_needed("acct", Shape::Text, 4 * MIN), ENOUGH - 3);
    }

    #[test]
    fn a_busy_service_buys_nothing() {
        let cover = Cover::default();
        cover.paid("acct", 0);
        for i in 1..=20 {
            cover.note(Shape::Text, i * MIN / 10);
        }
        assert_eq!(cover.decoys_needed("acct", Shape::Text, 3 * MIN), 0);
    }

    #[test]
    fn a_text_call_does_not_hide_a_picture() {
        let cover = Cover::default();
        cover.paid("acct", 0);
        for i in 1..=5 {
            cover.note(Shape::Text, i * MIN);
        }
        // Five text calls, and the first question draws a picture: still conspicuous.
        assert_eq!(cover.decoys_needed("acct", Shape::Picture, 6 * MIN), ENOUGH);
    }

    #[test]
    fn traffic_from_before_the_payment_is_not_cover() {
        let cover = Cover::default();
        for i in 1..=5 {
            cover.note(Shape::Text, i * MIN);
        }
        cover.paid("acct", 6 * MIN);
        assert_eq!(cover.decoys_needed("acct", Shape::Text, 7 * MIN), ENOUGH);
    }

    #[test]
    fn an_account_that_never_paid_is_never_covered() {
        let cover = Cover::default();
        assert_eq!(cover.decoys_needed("stranger", Shape::Text, MIN), 0);
    }

    #[test]
    fn a_payment_long_past_stops_being_an_anchor() {
        let cover = Cover::default();
        cover.paid("acct", 0);
        assert_eq!(cover.decoys_needed("acct", Shape::Text, COVER_KEEP_MS + MIN), 0);
    }
}
