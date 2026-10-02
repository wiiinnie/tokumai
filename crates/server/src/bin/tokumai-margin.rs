//! What is left of a subscription after everyone has been paid.
//!
//!     cargo run --release -p tokumai-server --bin tokumai-margin -- [--use 0.6] [--eurusd 1.10] …
//!
//! The plan prices and the allowances come from `subscription::TIERS`, and the exchange
//! rate between an allowance and a provider bill from `billing::TOKU_PER_USD` — so this
//! cannot drift away from what the enclave actually charges. Everything else is an
//! assumption, and every assumption is a flag, because the honest version of this
//! calculation is one somebody else can re-run with their own numbers.
//!
//! The structural fact worth knowing before reading any of the rows: **the model bill
//! cannot run away from us.** An allowance is denominated in provider cost — 100,000 TOKU
//! is one dollar of it — and a request is charged that cost times `MARGIN`. So a
//! subscriber who spends their whole allowance costs us, at the very most,
//! `allowance / MARGIN / TOKU_PER_USD` dollars, whatever mix of text and pictures they
//! chose. There is no usage pattern that turns a €10 plan into a €30 provider bill. What
//! is left over therefore depends on three things only: how much of the allowance is drawn,
//! what the payment rail keeps, and the fixed costs divided by however many people are
//! paying them.
//!
//! What this does NOT model: VAT registration thresholds (§19 UStG), corporate tax,
//! refunds and chargebacks, the audit and the lawyer, anyone's time.

use tokumai_core::billing::TOKU_PER_USD;
use tokumai_core::subscription::{yearly_cents, TIERS};

fn arg(name: &str, fallback: f64) -> f64 {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(fallback)
}

/// What a fully drawn allowance costs us at the provider, in USD.
fn provider_usd(toku: u64, margin: f64) -> f64 {
    toku as f64 / margin / TOKU_PER_USD as f64
}

struct Rail {
    name: &'static str,
    /// Share of the price the rail keeps, before anything else.
    cut: f64,
    /// Flat fee per payment, in EUR.
    flat_eur: f64,
}

fn main() {
    let margin = arg("--margin", 1.15);
    let eurusd = arg("--eurusd", 1.10);
    let vat = arg("--vat", 19.0) / 100.0; // included in the shown price
    let used = arg("--use", 0.6); // share of the allowance actually drawn
    let stripe_pct = arg("--stripe-pct", 1.5) / 100.0;
    let stripe_flat = arg("--stripe-flat", 0.25);
    let apple_pct = arg("--apple-pct", 15.0) / 100.0;

    // Fixed monthly costs, in EUR unless the name says otherwise.
    let aws_usd = arg("--aws-usd", 117.0); // c7g.xlarge on demand in Frankfurt, 24/7
    let ebs_usd = arg("--ebs-usd", 8.0); // the root volume and the book's own volume
    let egress_usd = arg("--egress-usd", 15.0); // data out of AWS
    let gateways = arg("--gateways", 4.0);
    let gateway_eur = arg("--gateway-eur", 15.0);
    let misc_eur = arg("--misc-eur", 25.0); // domain, mail, backups, the site
    // Session cover (enclave::cover): every call in the six hours after a payment is held
    // against other accounts' calls and topped up to five with decoys. In a thin hour that
    // is five decoys per call; how many calls fall into those hours is this.
    let cover_questions = arg("--cover-questions", 10.0);
    let picture_share = arg("--pictures", 0.08);

    let fixed_usd = aws_usd + ebs_usd + egress_usd + (gateways * gateway_eur + misc_eur) * eurusd;

    // A decoy, at the prices in pricing.json — the same two numbers tokumai-cover-need uses.
    let text_decoy = 30.0 / 1e6 * 0.3 + 256.0 / 1e6 * 2.5;
    let picture_decoy = 1_290.0 / 1e6 * 30.0 + 64.0 / 1e6 * 1.5;

    println!("Assumptions: EUR/USD {eurusd:.2}, VAT {:.0}% included in the shown price, {:.0}% of the", vat * 100.0, used * 100.0);
    println!("allowance drawn, MARGIN {margin:.2}. Card: {:.1}% + €{stripe_flat:.2}. App Store: {:.0}%.", stripe_pct * 100.0, apple_pct * 100.0);
    println!("Fixed: ${aws_usd:.0} instance + ${ebs_usd:.0} disk + ${egress_usd:.0} egress + {gateways:.0}×€{gateway_eur:.0} gateways + €{misc_eur:.0} = ${fixed_usd:.0} a month.\n");

    let rails = [Rail { name: "card", cut: stripe_pct, flat_eur: stripe_flat }, Rail { name: "App Store", cut: apple_pct, flat_eur: 0.0 }];

    println!("Per subscriber per month, in USD — before the fixed costs:\n");
    println!("{:>6} │ {:>9} │ {:>6} │ {:>6} │ {:>5} │ {:>6} │ {:>6} │ {:>7} │ {:>6}", "plan", "rail", "gross", "VAT", "fee", "net", "model", "left", "of net");
    println!("{:->6}─┼─{:->9}─┼─{:->6}─┼─{:->6}─┼─{:->5}─┼─{:->6}─┼─{:->6}─┼─{:->7}─┼─{:->6}", "", "", "", "", "", "", "", "", "");
    for (i, (toku, cents)) in TIERS.iter().enumerate() {
        for rail in &rails {
            let gross = *cents as f64 / 100.0;
            let net_of_vat = gross / (1.0 + vat);
            let fee = net_of_vat * rail.cut + rail.flat_eur;
            let net = (net_of_vat - fee) * eurusd;
            let model = provider_usd(*toku, margin) * used;
            let left = net - model;
            println!(
                "{:>5}€ │ {:>9} │ {:>6.2} │ {:>6.2} │ {:>5.2} │ {:>6.2} │ {:>6.2} │ {:>7.2} │ {:>5.0}%",
                cents / 100,
                rail.name,
                gross * eurusd,
                (gross - net_of_vat) * eurusd,
                fee * eurusd,
                net,
                model,
                left,
                if net > 0.0 { left / net * 100.0 } else { 0.0 },
            );
        }
        if i + 1 < TIERS.len() {
            println!("{:->6}─┼─{:->9}─┼─{:->6}─┼─{:->6}─┼─{:->5}─┼─{:->6}─┼─{:->6}─┼─{:->7}─┼─{:->6}", "", "", "", "", "", "", "", "", "");
        }
    }

    // The worst case that is still a customer: the whole allowance drawn, every month.
    println!("\nIf the allowance is drawn in full ({:.0}% instead of {:.0}%):", 100.0, used * 100.0);
    for (toku, cents) in TIERS.iter() {
        let gross = *cents as f64 / 100.0;
        let net_card = ((gross / (1.0 + vat)) * (1.0 - stripe_pct) - stripe_flat) * eurusd;
        let net_apple = (gross / (1.0 + vat)) * (1.0 - apple_pct) * eurusd;
        let model = provider_usd(*toku, margin);
        println!(
            "  €{:>2} plan: model ${model:>5.2} → ${:>5.2} left by card ({:>3.0}%), ${:>5.2} on the App Store ({:>3.0}%)",
            cents / 100,
            net_card - model,
            (net_card - model) / net_card * 100.0,
            net_apple - model,
            (net_apple - model) / net_apple * 100.0,
        );
    }

    // Break-even, and what the fixed costs cost per head.
    let entry = {
        let gross = TIERS[0].1 as f64 / 100.0;
        let net = ((gross / (1.0 + vat)) * (1.0 - stripe_pct) - stripe_flat) * eurusd;
        net - provider_usd(TIERS[0].0, margin) * used
    };
    println!("\nAn entry plan by card leaves ${entry:.2} a month, so the fixed costs need {:.0} of them.", (fixed_usd / entry).ceil());
    println!("\n{:>7} │ {:>10} │ {:>10} │ {:>10} │ {:>8}", "users", "left/user", "fixed/user", "per month", "margin");
    println!("{:->7}─┼─{:->10}─┼─{:->10}─┼─{:->10}─┼─{:->8}", "", "", "", "", "");
    for users in [25u32, 50, 100, 250, 500, 1_000, 2_500, 5_000] {
        let fixed_each = fixed_usd / users as f64;
        let each = entry - fixed_each;
        let gross_each = TIERS[0].1 as f64 / 100.0 * eurusd;
        println!("{users:>7} │ {entry:>9.2}$ │ {fixed_each:>9.2}$ │ {:>9.0}$ │ {:>7.0}%", each * users as f64, each / gross_each * 100.0);
    }

    let cover_worst = cover_questions * 5.0 * ((1.0 - picture_share) * text_decoy + picture_share * picture_decoy);
    println!("\nCover: five decoys per call in the six hours after a payment while nobody else is active.");
    println!("With {cover_questions:.0} calls in that window ({:.0}% pictures) that is at most {cover_worst:.3}$ per payment —", picture_share * 100.0);
    println!("{:.3}$ a month per subscriber in the worst case (one payment a month), and nothing once", cover_worst);
    println!("other people's traffic covers it (tokumai-cover-need: text from ~250 users, pictures from ~2,500).");
    println!("A yearly plan is twelve months less 10 % (€{:.2} at the entry tier), same costs.", yearly_cents(TIERS[0].1) as f64 / 100.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_allowance_is_capped_provider_spend() {
        // The whole point: 700,000 TOKU at MARGIN 1.15 can buy at most $6.09 of provider.
        assert!((provider_usd(700_000, 1.15) - 6.087).abs() < 0.01);
        // …and a bigger margin buys less, which is what a margin is.
        assert!(provider_usd(700_000, 1.30) < provider_usd(700_000, 1.15));
        assert_eq!(provider_usd(0, 1.15), 0.0);
    }
}
