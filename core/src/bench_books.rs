//! What a book size costs, measured rather than argued.
//!
//! Three numbers decide how big a ticketbook should be, and they pull in opposite
//! directions:
//!
//!   1. **Issuing** is ONE blind signature per book, whatever its size — so bigger books
//!      mean fewer signatures for the same money. This is why books exist at all.
//!   2. **Epoch material** carries a signature per coin INDEX, so it grows with the book
//!      size and every device downloads it once per epoch.
//!   3. **Spending** costs pairings PER COIN and is untouched by book size — but a tender
//!      carries the request's CEILING, not its price, so it is the number that scales with
//!      traffic.
//!
//! Run:  cargo test -p scrai-core --lib bench_books -- --ignored --nocapture
//!
//! Deliberately a test and not a criterion bench: it needs no dev-dependency, it prints a
//! table a human reads once before choosing a constant, and nobody wants it in CI.
#![cfg(test)]

use crate::coconut;
use crate::federation;
use std::time::Instant;

/// Seconds, to three decimals — the numbers here span milliseconds to seconds.
fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[test]
#[ignore = "benchmark: minutes, prints a table"]
fn what_a_book_size_costs() {
    // 10 is what ships today; the subscription draw needs something far larger, and the
    // question is where the material stops being worth the saved signatures.
    const SIZES: [u64; 5] = [10, 50, 100, 250, 500];
    // One month of the €10 tier, in 0.1 ¢ coins.
    const MONTH_COINS: u64 = 10_000;

    println!();
    println!("  book   books/month   bootstrap    issue 1     material   issue a month");
    println!("  ----   -----------   ---------   ---------   ---------   -------------");

    for size in SIZES {
        // The same date the mint stamps a fresh epoch with.
        const DAY: u64 = 86_400;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let exp = (((now / DAY) * DAY + coconut::BOOK_VALIDITY_DAYS * DAY) as u32).max(1);

        let t0 = Instant::now();
        let auths = federation::bootstrap(1, 1, size, exp, coconut::COIN_TOKU)
            .expect("bootstrap an authority of this size");
        let boot = t0.elapsed();
        let auth = &auths[0];

        // What every device downloads once per epoch: the per-index signatures. Measured
        // through the request the client really makes, not a hand-built struct.
        let keys = auth.handle(federation::FedRequest::Keys).expect("keys");
        let material = serde_json::to_vec(&keys).map(|v| v.len()).unwrap_or(0);

        // One withdrawal: a blinded request in, a blinded signature out.
        let user = coconut::new_user();
        let (req, _info) =
            coconut::make_withdrawal_request(user.secret_key(), exp, coconut::DEFAULT_T_TYPE)
                .expect("withdrawal request");
        let withdraw = federation::FedRequest::Withdraw {
            user_pk: user.public_key(),
            req,
            denom_toku: coconut::COIN_TOKU,
            expiration_date: exp,
        };
        let t1 = Instant::now();
        let _ = auth.handle(withdraw).expect("issue one book");
        let issue = t1.elapsed();

        let books = MONTH_COINS.div_ceil(size);
        println!(
            "  {:>4}   {:>11}   {:>7.0}ms   {:>7.1}ms   {:>6.0} KB   {:>10.1}s",
            size,
            books,
            ms(boot),
            ms(issue),
            material as f64 / 1024.0,
            ms(issue) * books as f64 / 1000.0,
        );
    }
    println!();
    println!("  bootstrap is once per epoch on the SERVER; material is once per epoch per DEVICE.");
}

#[test]
#[ignore = "benchmark: minutes, prints a table"]
fn what_a_request_costs_the_server() {
    // The ceilings a tender actually carries, in 0.1 ¢ coins — a request puts its CEILING
    // on the table, not its price, so these are the numbers that scale with traffic.
    const CASES: [(&str, u64); 4] =
        [("text answer", 25), ("picture 6 ct", 60), ("picture 9 ct", 90), ("4K ceiling", 250)];
    // Big enough that no case is clipped; the testkit's 32-coin book is not.
    const BOOK: u64 = 256;

    const DAY: u64 = 86_400;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let exp = (((now / DAY) * DAY + coconut::BOOK_VALIDITY_DAYS * DAY) as u32).max(1);
    // The client spends against the epoch's own date, a day back — a date far from the
    // expiration is refused ("given spend date is too early").
    let spend_date = exp.saturating_sub(DAY as u32);

    let auths = federation::bootstrap(1, 1, BOOK, exp, coconut::COIN_TOKU).expect("bootstrap");
    let auth = &auths[0];
    let keys = match auth.handle(federation::FedRequest::Keys).expect("keys") {
        federation::FedResponse::Keys { vk, coin_sigs, date_sigs, total_coins, .. } => {
            crate::purse::EpochKeys {
                vk,
                coin_sigs,
                date_sigs,
                expiration_date: exp,
                total_coins,
                denom_toku: coconut::COIN_TOKU,
            }
        }
        _ => panic!("expected Keys"),
    };

    println!();
    println!("  request          coins    verify      per coin");
    println!("  -------------   ------   --------   ----------");
    for (label, coins) in CASES {
        // A fresh book per case: spending advances the counter, and a case must not be
        // charged for the one before it.
        let user = coconut::new_user();
        let (req, info) =
            coconut::make_withdrawal_request(user.secret_key(), exp, coconut::DEFAULT_T_TYPE)
                .expect("withdrawal request");
        let blinded = match auth
            .handle(federation::FedRequest::Withdraw {
                user_pk: user.public_key(),
                req,
                denom_toku: coconut::COIN_TOKU,
                expiration_date: exp,
            })
            .expect("issue")
        {
            federation::FedResponse::Withdraw { blinded } => blinded,
            _ => panic!("expected a Withdraw reply"),
        };
        // The client's own two steps: verify the share, then aggregate it into a wallet.
        let share = coconut::verify_share(&keys.vk, user.secret_key(), &blinded, &info, 1)
            .expect("the share verifies");
        let cred = coconut::aggregate(&keys.vk, user.secret_key(), &[share], &info)
            .expect("shares aggregate");
        let mut purse = crate::purse::Purse::new(cred, user, BOOK, exp, coconut::COIN_TOKU);

        let notes = purse
            .spend_tender(&keys, &crate::tender::plan_coins(coins), spend_date)
            .expect("spend_tender");

        let t = Instant::now();
        for note in &notes {
            let pi = note.pay_info().expect("pay_info");
            coconut::verify(&note.payment, &keys.vk, &pi, note.spend_date).expect("verify");
        }
        let took = t.elapsed();
        println!(
            "  {:<13}   {:>6}   {:>6.0}ms   {:>7.2}ms",
            label,
            coins,
            ms(took),
            ms(took) / coins as f64
        );
    }
    println!();
    println!("  a tender carries the CEILING, so this is what the server pays per request,");
    println!("  not what the answer cost.");
}
