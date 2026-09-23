//! How many people have to be using the service before a first question hides by itself.
//!
//!     cargo run --release -p tokumai-server --bin tokumai-cover-need -- [--questions 15] [--pictures 0.08] [--window 5] [--enough 3]
//!
//! The enclave buys a decoy only when nobody else's traffic covered a new customer's first
//! question (`enclave::cover`). That threshold — how many calls of the same shape count as
//! enough — should come from a number rather than from taste, and so should the answer to
//! "when does this stop costing anything".
//!
//! The model is the simplest one that fits: calls arrive independently, so the count in a
//! window of T minutes is Poisson with mean λT, where λ = users × questions per day. That
//! is generous to us in one way (real traffic clusters by daytime, so a quiet night is
//! quieter than this says) and harsh in another (a returning user's burst of questions is
//! several calls, not one). Its job is to size the threshold, not to predict revenue.
//!
//! What it prints is the share of first questions that need a decoy, and what that costs,
//! for text and for pictures separately — because a text call does not hide a picture, and
//! pictures are both rarer and dearer.

/// What one decoy costs, in dollars, at the prices in pricing.json (2026-09).
/// Text: gemini-3.5-flash-lite, ~30 tokens in, 256 out → 0.3/M in, 2.5/M out.
const TEXT_DECOY_USD: f64 = 30.0 / 1e6 * 0.3 + 256.0 / 1e6 * 2.5;
/// Picture: gemini-3.1-flash-lite-image at 1K → about 1,290 image tokens at 30/M.
const PICTURE_DECOY_USD: f64 = 1_290.0 / 1e6 * 30.0 + 64.0 / 1e6 * 1.5;

/// P(X < k) for X ~ Poisson(mean): the chance that fewer than `k` calls turned up.
fn fewer_than(k: u32, mean: f64) -> f64 {
    let mut term = (-mean).exp();
    let mut sum = term;
    for i in 1..k {
        term *= mean / i as f64;
        sum += term;
    }
    sum.min(1.0)
}

fn arg(name: &str, fallback: f64) -> f64 {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(fallback)
}

fn main() {
    let questions = arg("--questions", 15.0); // per active user per day
    let pictures = arg("--pictures", 0.08); // share of questions that draw something
    let window = arg("--window", 5.0); // minutes between paying and asking the first thing
    let enough = arg("--enough", 3.0) as u32; // calls of the same shape that count as cover

    println!("A first question hides when {enough} other call(s) of its shape arrive within {window:.0} min.");
    println!("Assuming {questions:.0} questions per active user per day, {:.0}% of them pictures.\n", pictures * 100.0);
    println!("{:>7} │ {:>12} │ {:>12} │ {:>10} │ {:>12}", "users", "text decoy", "picture decoy", "per 100", "if half pay");
    println!("{:->7}─┼─{:->12}─┼─{:->12}─┼─{:->10}─┼─{:->12}", "", "", "", "", "");

    for users in [1u32, 5, 10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000] {
        // Calls per minute, by shape.
        let per_minute = users as f64 * questions / (24.0 * 60.0);
        let text = per_minute * (1.0 - pictures) * window;
        let picture = per_minute * pictures * window;
        // The chance that cover did NOT turn up, which is when a decoy is bought.
        let text_short = fewer_than(enough, text);
        let picture_short = fewer_than(enough, picture);
        // What 100 purchases cost in decoys, if a picture-first customer is as likely as
        // the share of pictures overall.
        let per_100 = 100.0 * ((1.0 - pictures) * text_short * TEXT_DECOY_USD + pictures * picture_short * PICTURE_DECOY_USD);
        println!(
            "{users:>7} │ {:>11.0}% │ {:>11.0}% │ {:>9.2}$ │ {:>11.2}$",
            text_short * 100.0,
            picture_short * 100.0,
            per_100,
            per_100 * users as f64 / 200.0
        );
    }

    println!("\nOne decoy costs {:.4}$ (text) or {:.4}$ (a 1K picture).", TEXT_DECOY_USD, PICTURE_DECOY_USD);
    println!("\"if half pay\" is the monthly decoy bill when half of those users join in that month —");
    println!("the worst case, since a purchase is what starts the watching.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_poisson_tail_is_the_one_we_mean() {
        // Nothing expected: fewer than one call is certain.
        assert!((fewer_than(1, 0.0) - 1.0).abs() < 1e-9);
        // A mean of 3 leaves about 42 % of windows with fewer than 3 calls.
        assert!((fewer_than(3, 3.0) - 0.4232).abs() < 1e-3);
        // Plenty of traffic: a shortfall becomes vanishing.
        assert!(fewer_than(3, 50.0) < 1e-15);
    }

    #[test]
    fn a_decoy_is_cheap_in_text_and_not_in_pictures() {
        assert!(TEXT_DECOY_USD < 0.001);
        assert!(PICTURE_DECOY_USD > 0.03 && PICTURE_DECOY_USD < 0.05);
    }
}
