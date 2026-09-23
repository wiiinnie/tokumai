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
//! When the new person finally asks something, the enclave looks at how much cover of the
//! same shape has piled up — a text call does not hide a picture — and only if there is
//! none does it buy a decoy: one call of matching shape, offset the way ordinary traffic
//! is offset. Nobody's question is ever held back, so no one waits for this.
//!
//! The cost falls to zero as the service fills up, which is the point: the decoys are only
//! bought while there is nobody to hide among.
//!
//! What it does not do: with a single active account there is nobody to hide among at all,
//! and one decoy against an observer who can repeat the observation for months is thin.
//! It blunts the sharpest edge; it does not make a first purchase unlinkable.

use std::collections::HashMap;
use std::sync::Mutex;

/// What a call looks like from outside: a few kilobytes back, or several megabytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Text,
    Picture,
}

/// How many calls of the same shape, since the payment, count as enough cover.
const ENOUGH: usize = 3;

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

    /// Whether this account's call needs a decoy beside it. Asked once per account: after
    /// the first question the payment has stopped being an anchor, whether we covered it
    /// with someone else's traffic or with our own money.
    pub fn needs_decoy(&self, account: &str, shape: Shape, now_ms: u64) -> bool {
        let Ok(mut waiting) = self.waiting.lock() else { return false };
        let Some(paid_at) = waiting.remove(account) else { return false };
        if now_ms.saturating_sub(paid_at) >= COVER_KEEP_MS {
            return false; // long enough ago that the payment no longer points at anything
        }
        let cover = self
            .seen
            .lock()
            .map(|seen| seen.iter().filter(|(at, s)| *s == shape && *at >= paid_at).count())
            .unwrap_or(0);
        cover < ENOUGH
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
    fn a_first_question_in_an_empty_room_is_given_a_decoy() {
        let cover = Cover::default();
        cover.paid("acct", 0);
        // Nothing else happened in between.
        assert!(cover.needs_decoy("acct", Shape::Text, 2 * MIN));
        // Asked once only: the payment has stopped being an anchor.
        assert!(!cover.needs_decoy("acct", Shape::Text, 3 * MIN));
    }

    #[test]
    fn other_peoples_traffic_makes_the_decoy_unnecessary() {
        let cover = Cover::default();
        cover.paid("acct", 0);
        for i in 1..=3 {
            cover.note(Shape::Text, i * MIN);
        }
        assert!(!cover.needs_decoy("acct", Shape::Text, 4 * MIN));
    }

    #[test]
    fn a_text_call_does_not_hide_a_picture() {
        let cover = Cover::default();
        cover.paid("acct", 0);
        for i in 1..=5 {
            cover.note(Shape::Text, i * MIN);
        }
        // Five text calls, and the first question draws a picture: still conspicuous.
        assert!(cover.needs_decoy("acct", Shape::Picture, 6 * MIN));
    }

    #[test]
    fn traffic_from_before_the_payment_is_not_cover() {
        let cover = Cover::default();
        for i in 1..=5 {
            cover.note(Shape::Text, i * MIN);
        }
        cover.paid("acct", 6 * MIN);
        assert!(cover.needs_decoy("acct", Shape::Text, 7 * MIN));
    }

    #[test]
    fn an_account_that_never_paid_is_never_covered() {
        let cover = Cover::default();
        assert!(!cover.needs_decoy("stranger", Shape::Text, MIN));
    }

    #[test]
    fn a_payment_long_past_stops_being_an_anchor() {
        let cover = Cover::default();
        cover.paid("acct", 0);
        assert!(!cover.needs_decoy("acct", Shape::Text, COVER_KEEP_MS + MIN));
    }
}
