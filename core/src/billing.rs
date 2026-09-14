// ---------------------------------------------------------------------------
// billing.rs — the pricing MATH, ported from src/billing.ts (byte-for-byte
// behaviour, so the Rust server prices an exchange exactly like the TS server did).
//
// This is the pure computation: provider cost in USD → TOKU, noise-safe rounding,
// margin, and a per-request floor. The pricing TABLE (which model costs what) is a
// separate data port; here `ModelPrice` is passed in explicitly.
// ---------------------------------------------------------------------------

use crate::coconut::TOKU_PER_USD;

/// Token usage for one exchange (provider-reported or estimated).
#[derive(Debug, Clone, Copy, Default)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cached_input: u64,
    pub audio_input: u64,
    /// Output tokens that ARE a generated image (Gemini reports them under
    /// `candidatesTokensDetails` with modality IMAGE). Billed at the model's image
    /// output rate; `output` (text + thinking) bills at the (much cheaper) text rate.
    pub output_image: u64,
    /// Web-search queries the model executed for live grounding (Gemini). Billed
    /// separately, per query — NOT a token count, so `cost_usd` ignores it.
    pub grounding_queries: u64,
    /// True when the provider reported no usage and these counts are OUR estimate
    /// (chars/4, typical image size). Surfaced to the client as `billing.estimated`.
    pub estimated: bool,
}

/// A model's provider price, in USD per 1,000,000 tokens.
#[derive(Debug, Clone, Copy)]
pub struct ModelPrice {
    pub input: f64,
    /// Output rate. For image models this is the IMAGE-token rate (e.g. $60/1M on
    /// Nano Banana 2) and `output_text` carries the text/thinking rate; for text
    /// models it is simply the output rate and `output_text` is None.
    pub output: f64,
    /// Rate for text + thinking output tokens on models whose `output` is an image
    /// rate. None → text output bills at `output` (text models).
    pub output_text: Option<f64>,
    /// Cheaper rate for cached input tokens; falls back to `input` if None.
    pub cached: Option<f64>,
    /// Rate for audio input tokens; falls back to `input` if None.
    pub audio: Option<f64>,
    /// True when this price came from the fallback default, not a real table entry.
    pub fallback: bool,
    /// Business tier — how this model is offered (see `Tier`).
    pub tier: Tier,
    /// Flat USD price per generated image, for image models whose providers
    /// report zero tokens (e.g. Cloudflare flux). None for text models.
    pub per_image: Option<f64>,
}

/// How a model is offered to users:
/// - `FreeTier`: a paid model the operator's API key uses on a daily free
///   allowance; billed to users at a REDUCED rate, and past the allowance the
///   provider's error is passed through (until API-side billing is enabled).
/// - `Paid`: always billed at full retail.
///
/// There is deliberately no "genuinely free" tier any more. It existed for the
/// keyless test providers (pollinations), and it was the one request shape the
/// server answered with NO signature, NO session and NO reserve. Those providers
/// were removed before mainnet (2026-09-04) and the tier went with them — every
/// request now costs something and is therefore authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tier {
    FreeTier,
    #[default]
    Paid,
}

impl Tier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Tier::FreeTier => "free-tier",
            Tier::Paid => "paid",
        }
    }
}

/// The billing frame for one exchange.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BillingFrame {
    /// Provider cost to us, in TOKU, kept to 4 decimals (no rounding accretion).
    pub cost_toku: f64,
    /// What the user pays: whole TOKU, margin applied, floored — never free once a
    /// request actually reached the provider.
    pub price_toku: u64,
    pub fallback_price: bool,
    pub estimated: bool,
}

/// Provider cost in USD for one exchange.
pub fn cost_usd(usage: &TokenUsage, price: &ModelPrice) -> f64 {
    let cached_rate = price.cached.unwrap_or(price.input);
    let audio_rate = price.audio.unwrap_or(price.input);
    let text_out_rate = price.output_text.unwrap_or(price.output);
    (usage.input as f64 * price.input
        + usage.cached_input as f64 * cached_rate
        + usage.audio_input as f64 * audio_rate
        + usage.output as f64 * text_out_rate
        + usage.output_image as f64 * price.output)
        / 1_000_000.0
}

/// Round a TOKU amount UP at 4 decimals, WITHOUT inventing money out of float noise.
///
/// The naive `(x * 10_000).ceil() / 10_000` is wrong: e.g. a value that computes to
/// `17100.000000000004` would ceil to `17101` and invent a cost the provider never
/// charged. We first normalise to 12 significant figures (as JS `toPrecision(12)`
/// does), dropping the noise while leaving genuine fractions intact, then ceil.
pub fn ceil_toku(scrai: f64) -> f64 {
    let scaled = scrai * 10_000.0;
    let normalized: f64 = format!("{scaled:.11e}").parse().unwrap_or(scaled);
    normalized.ceil() / 10_000.0
}

/// Rough token count when a provider reports none. ~4 chars/token.
pub fn estimate_tokens(chars: u64) -> u64 {
    chars.div_ceil(4)
}

/// Clamp a margin the way the TS server does: must be finite and ≥ 1, else 1.0.
/// Google's published output tokens per generated image, by requested size
/// (`generationConfig.imageConfig.imageSize` on Gemini 3.x image models). Nano Banana
/// (2.5) takes no size and always returns a 1K picture of 1290 tokens.
pub const IMAGE_SIZES: [(&str, u64); 4] = [("512", 747), ("1K", 1120), ("2K", 1680), ("4K", 2520)];
pub const DEFAULT_IMAGE_SIZE: &str = "1K";

/// Output tokens of ONE picture at `size`. An unknown size bills as 1K.
pub fn image_tokens_for(size: &str) -> u64 {
    IMAGE_SIZES.iter().find(|(s, _)| *s == size).map(|(_, t)| *t).unwrap_or(1120)
}

/// Safe upper bound on the input tokens one attachment bills as. Gemini tiles a large
/// image into ~hundreds of tokens and a PDF page costs ~258+; this over-reserves rather
/// than risk billing above the ceiling.
pub const ATTACHMENT_INPUT_TOKENS: u64 = 4096;

/// What one request could cost at worst, in tokens.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ceiling {
    /// Input tokens, counted as BYTES (a byte-level BPE token decodes to at least one
    /// byte, so the byte count cannot undercount) plus a flat budget per attachment.
    pub in_tokens: u64,
    /// Answer budget plus the thinking budget — both bill at the text rate.
    pub out_tokens: u64,
    /// Output tokens of the picture this request may generate; 0 for text models and
    /// for models that do not bill an image separately.
    pub image_tokens: u64,
}

/// Worst-case retail price of one request, in TOKU: what the server reserves, and
/// therefore what a client paying with coins has to put on the table.
///
/// ONE definition, used by both sides. The client used to estimate this with its own
/// formula and got image models wrong by a factor of three — it tendered 1300 TOKU for a
/// 4368 TOKU picture and the request was refused with coins sitting right there
/// (2026-09-14). A second formula is a second answer; there is only room for one.
pub fn ceiling_toku(price: &ModelPrice, margin: f64, c: &Ceiling) -> u64 {
    let retail = |usd_per_million: f64| ceil_toku(usd_per_million * TOKU_PER_USD as f64 * clamp_margin(margin)).ceil();
    // Token-billed image models (Nano Banana): text + thinking reserve at the TEXT rate,
    // the picture at the IMAGE rate — the same split settle() bills.
    let (text_out_rate, image_tokens) = match price.output_text {
        Some(t) => (t, c.image_tokens),
        None => (price.output, 0),
    };
    ((c.in_tokens as f64 * retail(price.input)
        + c.out_tokens as f64 * retail(text_out_rate)
        + image_tokens as f64 * retail(price.output))
        / 1_000_000.0)
        .ceil() as u64
}

/// Retail TOKU for ONE generated image on a model whose provider reports no tokens for
/// it (a flat per-image price); 0 for text models and for token-billed image models.
pub fn per_image_toku(price: &ModelPrice, margin: f64) -> u64 {
    match price.per_image {
        Some(usd) if usd > 0.0 => ceil_toku(usd * TOKU_PER_USD as f64 * clamp_margin(margin)).ceil() as u64,
        _ => 0,
    }
}

/// Input tokens of a `[{role, content, attachments}]` message array, the way the ceiling
/// must count them. Bytes, not chars/4: an adversarial multibyte prompt tokenises far
/// above chars/4, and nobody may ever be charged above the ceiling.
pub fn ceiling_input_tokens(messages: &serde_json::Value) -> u64 {
    messages
        .as_array()
        .map(|a| {
            a.iter()
                .map(|m| {
                    let text = m.get("content").and_then(|c| c.as_str()).unwrap_or("").len() as u64;
                    let atts = m.get("attachments").and_then(|x| x.as_array()).map(|x| x.len()).unwrap_or(0) as u64;
                    text + atts * ATTACHMENT_INPUT_TOKENS
                })
                .sum()
        })
        .unwrap_or(0)
}

pub fn clamp_margin(margin: f64) -> f64 {
    if margin.is_finite() && margin >= 1.0 {
        margin
    } else {
        1.0
    }
}

/// Build the billing frame. `cost` keeps 4 decimals; `price` is whole TOKU, rounded
/// up with margin, floored at `min_charge` — a request that reached the provider is
/// never free (unless the model itself is free AND min_charge is 0).
pub fn compute_billing(
    price: &ModelPrice,
    usage: &TokenUsage,
    margin: f64,
    min_charge: u64,
    estimated: bool,
) -> BillingFrame {
    let cost_toku = ceil_toku(cost_usd(usage, price) * TOKU_PER_USD as f64);
    let billable =
        usage.input + usage.cached_input + usage.audio_input + usage.output + usage.output_image;
    let price_toku = if billable > 0 {
        ((cost_toku * clamp_margin(margin)).ceil() as u64).max(min_charge)
    } else {
        0
    };
    BillingFrame {
        cost_toku,
        price_toku,
        fallback_price: price.fallback,
        estimated,
    }
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    const P: ModelPrice = ModelPrice {
        input: 0.10, // $0.10 / 1M input
        output: 0.40,
        output_text: None,
        cached: None,
        audio: None,
        fallback: false,
        tier: Tier::Paid,
        per_image: None,
    };

    #[test]
    fn cost_and_price_with_margin() {
        // 1M input tokens @ $0.10/1M = $0.10 → 10_000 TOKU cost; ×1.1 margin → 11_000
        let u = TokenUsage { input: 1_000_000, ..Default::default() };
        let f = compute_billing(&P, &u, 1.1, 0, false);
        assert_eq!(f.cost_toku, 10_000.0);
        assert_eq!(f.price_toku, 11_000);
        assert!(!f.fallback_price);
    }

    #[test]
    fn empty_usage_is_free_but_tiny_paid_is_floored_to_one() {
        // no tokens → 0
        assert_eq!(compute_billing(&P, &TokenUsage::default(), 1.1, 0, false).price_toku, 0);
        // 1 input token @ $0.10/1M = 0.01 TOKU cost → ceil(0.01×1.1)=ceil(0.011)=1
        let tiny = TokenUsage { input: 1, ..Default::default() };
        let f = compute_billing(&P, &tiny, 1.1, 0, false);
        assert_eq!(f.cost_toku, 0.01);
        assert_eq!(f.price_toku, 1);
    }

    #[test]
    fn zero_priced_model_costs_nothing() {
        let free = ModelPrice { input: 0.0, output: 0.0, ..P };
        let u = TokenUsage { input: 1000, output: 1000, ..Default::default() };
        let f = compute_billing(&free, &u, 1.1, 0, false);
        assert_eq!(f.cost_toku, 0.0);
        assert_eq!(f.price_toku, 0);
    }

    #[test]
    fn fallback_flag_and_min_charge() {
        let fb = ModelPrice { fallback: true, ..P };
        let u = TokenUsage { input: 1, ..Default::default() };
        // min_charge floor lifts a tiny cost up to 3
        let f = compute_billing(&fb, &u, 1.1, 3, false);
        assert!(f.fallback_price);
        assert_eq!(f.price_toku, 3);
    }

    #[test]
    fn ceil_scrai_rounds_up_but_never_inflates_from_noise() {
        // exact 4-decimal amounts must round to themselves (no float-noise inflation)
        for x in [0.0, 1.71, 133.4, 155.0, 10_000.0] {
            assert_eq!(ceil_toku(x), x, "ceil_toku inflated {x}");
        }
        // a genuine 5th-decimal fraction rounds UP to 4 decimals
        assert_eq!(ceil_toku(1.234_55), 1.2346);
        assert_eq!(ceil_toku(2.795_1), 2.7951);
    }

    #[test]
    fn estimate_tokens_is_four_chars_each() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(4), 1);
        assert_eq!(estimate_tokens(5), 2); // ceil
        assert_eq!(estimate_tokens(400), 100);
    }

    #[test]
    fn cached_and_audio_rates_default_to_input() {
        let u = TokenUsage { cached_input: 1_000_000, audio_input: 1_000_000, ..Default::default() };
        // both fall back to the $0.10 input rate → 0.10 + 0.10 = $0.20 → 20_000 TOKU
        assert_eq!(cost_usd(&u, &P), 0.20);
    }

    #[test]
    fn image_models_bill_text_and_thinking_at_the_text_rate() {
        // Nano Banana 2 shape: image tokens $60/1M, text/thinking $3/1M.
        let nb2 = ModelPrice { input: 0.5, output: 60.0, output_text: Some(3.0), ..P };
        // one 1K image (1120 tokens) + 200 text + 1500 thinking tokens
        let u = TokenUsage { output_image: 1120, output: 1700, ..Default::default() };
        let usd = cost_usd(&u, &nb2);
        // 1120×60 + 1700×3 = 67_200 + 5_100 = 72_300 per-million → $0.0723
        assert!((usd - 0.0723).abs() < 1e-12, "{usd}");
        // billing it all at the image rate would have been 2820×60 = $0.1692 — the bug.
        assert!(usd < 0.08);
        let f = compute_billing(&nb2, &u, 1.0, 0, false);
        assert_eq!(f.cost_toku, 7230.0);
        assert_eq!(f.price_toku, 7230);
    }

    #[test]
    fn output_image_tokens_count_as_billable_even_without_text() {
        let nb2 = ModelPrice { output: 60.0, output_text: Some(3.0), ..P };
        let u = TokenUsage { output_image: 1120, ..Default::default() };
        assert!(compute_billing(&nb2, &u, 1.4, 0, false).price_toku > 0);
    }

    #[test]
    fn margin_below_one_is_clamped() {
        let u = TokenUsage { input: 1_000_000, ..Default::default() };
        // a nonsense margin of 0.5 must not reduce the price below cost
        let f = compute_billing(&P, &u, 0.5, 0, false);
        assert_eq!(f.price_toku, 10_000); // clamped to ×1.0
    }
}
