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
//! it takes to make up the difference.
//!
//! The decoys and the real call share one spread of a few seconds (`SPREAD_MS`), and the
//! real call takes a random place in it: so it is first among them no more often than any
//! other, goes to the same provider (an OpenAI question's decoys go to OpenAI, with the
//! moderation call in front, as the real one has), and is of the same shape. That holds a
//! covered question back by a second or two on average, only while decoys are bought —
//! an uncovered one is never held. (Until 2026-10-06 every decoy was scheduled strictly
//! after the real call, and to Google whatever the real call's provider: the first egress
//! after a payment was the payer's, every time — audit M3.)
//!
//! And not only the first question (session cover, 2026-10-02): a person who has just
//! paid asks several things in a row, and in a thin hour "payment, then an afternoon of
//! calls" says the payer kept asking even when no single call can be pointed at. So the
//! account stays armed for the whole window, and every one of its calls in it is held
//! against the other accounts' calls since its previous one — each stands among six, not
//! only the first. What that still leaves is the existence of the session itself; only a
//! crowd hides that, and the pitch says so.
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

/// How long a payment keeps its account armed: longer than any plausible gap between
/// paying and asking the first question, and the session that follows it. Older than this,
/// the payment has stopped being an anchor.
const COVER_KEEP_MS: u64 = 6 * 3_600_000;

/// The most calls we remember. A busy enclave needs no more than this to answer "was
/// there cover", and the list is only ever read backwards.
const REMEMBER: usize = 512;

/// The spread, in milliseconds, within which a covered call and its decoys all go out,
/// each at a moment of its own. Three seconds: a fraction of a round trip over the mixnet,
/// and wide enough that calls a few hundred milliseconds apart look like what they are
/// meant to look like — several people, not one arrangement.
pub const SPREAD_MS: u64 = 3_000;

/// The moments for one covered call and `decoys` decoys: the first is the real call's,
/// the rest the decoys', all drawn alike — so nothing about the order says which is which.
pub fn moments(decoys: usize) -> (u64, Vec<u64>) {
    let mut all: Vec<u64> = (0..=decoys).map(|_| rand::random::<u64>() % SPREAD_MS).collect();
    let mine = all.swap_remove(rand::random::<usize>() % all.len());
    (mine, all)
}

#[derive(Default)]
pub struct Cover {
    /// When each recent provider call happened, and what it looked like.
    seen: Mutex<Vec<(u64, Shape)>>,
    /// Accounts within the window of a payment: when they paid, and when they last asked
    /// something (the payment itself, until the first question).
    waiting: Mutex<HashMap<String, (u64, u64)>>,
}

impl Cover {
    /// A plan was granted, or credit was added: this account's first question from here on
    /// is the one that could be tied to the payment.
    pub fn paid(&self, account: &str, now_ms: u64) {
        if let Ok(mut waiting) = self.waiting.lock() {
            waiting.retain(|_, (paid, _)| now_ms.saturating_sub(*paid) < COVER_KEEP_MS);
            waiting.insert(account.to_string(), (now_ms, now_ms));
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
    /// its shape since the account's previous call (or its payment) up to [`ENOUGH`],
    /// counting what other people provided in between. Asked for every call inside the
    /// window; the account's own earlier calls are not cover for its later ones.
    pub fn decoys_needed(&self, account: &str, shape: Shape, now_ms: u64) -> usize {
        let Ok(mut waiting) = self.waiting.lock() else { return 0 };
        let Some((paid_at, since)) = waiting.get(account).copied() else { return 0 };
        if now_ms.saturating_sub(paid_at) >= COVER_KEEP_MS {
            waiting.remove(account);
            return 0; // long enough ago that the payment no longer points at anything
        }
        // Strictly after the previous anchor: that moment's own call is this account's.
        let cover = self
            .seen
            .lock()
            .map(|seen| seen.iter().filter(|(at, s)| *s == shape && (*at > since || (*at == since && since == paid_at))).count())
            .unwrap_or(0);
        waiting.insert(account.to_string(), (paid_at, now_ms));
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
        cover.note(Shape::Text, 2 * MIN);
        // The session goes on in the same empty room: every call is covered, and the
        // account's own previous call is not its cover.
        assert_eq!(cover.decoys_needed("acct", Shape::Text, 3 * MIN), ENOUGH);
        cover.note(Shape::Text, 3 * MIN);
        // Other people arrive between two of its calls: those count.
        for i in 1..=5 {
            cover.note(Shape::Text, 3 * MIN + i * 1_000);
        }
        assert_eq!(cover.decoys_needed("acct", Shape::Text, 4 * MIN), 0);
        // The window closes six hours after the payment, not after the last call.
        assert_eq!(cover.decoys_needed("acct", Shape::Text, 7 * 60 * MIN), 0);
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

    /// The real call is first among its decoys about as often as any of them is — never
    /// always, which is what gave the first question away.
    #[test]
    fn the_real_call_takes_a_random_place_among_the_decoys() {
        let mut first = 0;
        for _ in 0..600 {
            let (mine, decoys) = moments(ENOUGH);
            assert_eq!(decoys.len(), ENOUGH);
            assert!(mine < SPREAD_MS && decoys.iter().all(|d| *d < SPREAD_MS));
            if decoys.iter().all(|d| *d >= mine) {
                first += 1;
            }
        }
        // One in six on average (100 of 600); far from six hundred, and not none.
        assert!((40..=200).contains(&first), "first {first} times of 600");
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
