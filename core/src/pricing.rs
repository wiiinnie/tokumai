// ---------------------------------------------------------------------------
// pricing.rs — the pricing TABLE: per-model provider prices (USD / 1M tokens),
// parsed from pricing.json (schema "scrambler/pricing@1"). The billing MATH lives
// in billing.rs; this just answers "what does model X cost?" and carries the table
// version so a billing frame can say which prices applied.
//
// An unlisted model gets the conservative `default` (fallback = true): we'd rather
// over-charge slightly than under-charge and lose money. Margin is NOT in the table
// — it's applied at billing time from the server's MARGIN env.
// ---------------------------------------------------------------------------

use std::collections::HashMap;

use serde::Deserialize;

use crate::billing::{ModelPrice, Tier};

/// One row of pricing.json (`in`/`out`/`cached_in`/`audio_in` = USD per 1M tokens,
/// `out_text` = text/thinking output rate for image models whose `out` is the
/// image-token rate, `per_image` = USD per generated image, `tier` = "free" |
/// "free-tier" | absent for paid). Unknown fields (label, note, floating, …) are ignored.
#[derive(Deserialize)]
struct RawPrice {
    #[serde(rename = "in")]
    input: f64,
    #[serde(rename = "out")]
    output: f64,
    #[serde(default, rename = "out_text")]
    out_text: Option<f64>,
    #[serde(default, rename = "cached_in")]
    cached_in: Option<f64>,
    #[serde(default, rename = "audio_in")]
    audio_in: Option<f64>,
    #[serde(default)]
    fallback: bool,
    #[serde(default)]
    tier: Option<String>,
    #[serde(default)]
    per_image: Option<f64>,
    /// Human-facing marketing name (e.g. "Nano Banana") — carried alongside the price
    /// so the picker can show it instead of the bare API id. Not part of ModelPrice
    /// (which is Copy); the table keeps it in a separate map.
    #[serde(default)]
    label: Option<String>,
}

impl RawPrice {
    fn to_model_price(&self, force_fallback: bool) -> ModelPrice {
        // Unknown tier strings deliberately land on Paid — mistyping a tier must
        // never accidentally give a model away for free.
        let tier = match self.tier.as_deref() {
            Some("free") => Tier::Free,
            Some("free-tier") => Tier::FreeTier,
            _ => Tier::Paid,
        };
        ModelPrice {
            input: self.input,
            output: self.output,
            output_text: self.out_text,
            cached: self.cached_in,
            audio: self.audio_in,
            fallback: force_fallback || self.fallback,
            tier,
            per_image: self.per_image,
        }
    }
}

#[derive(Deserialize)]
struct RawTable {
    #[serde(default)]
    version: String,
    default: RawPrice,
    models: HashMap<String, RawPrice>,
}

/// The parsed pricing table: a version, a per-model price map, and a fallback default.
pub struct PricingTable {
    version: String,
    default: ModelPrice,
    models: HashMap<String, ModelPrice>,
    labels: HashMap<String, String>,
}

impl PricingTable {
    /// Parse pricing.json. Errors only on malformed JSON / missing `default`.
    pub fn parse(json: &str) -> Result<PricingTable, String> {
        let raw: RawTable = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let models = raw
            .models
            .iter()
            .map(|(k, v)| (k.clone(), v.to_model_price(false)))
            .collect();
        let labels = raw
            .models
            .iter()
            .filter_map(|(k, v)| v.label.clone().map(|l| (k.clone(), l)))
            .collect();
        Ok(PricingTable {
            version: raw.version,
            default: raw.default.to_model_price(true),
            models,
            labels,
        })
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    /// The marketing name for `model` (e.g. "Nano Banana"), if `pricing.json` gives one.
    pub fn label(&self, model: &str) -> Option<&str> {
        self.labels.get(model).map(|s| s.as_str())
    }

    /// Price for `model`, or the conservative default (fallback = true) if unlisted.
    pub fn price(&self, model: &str) -> ModelPrice {
        self.models.get(model).copied().unwrap_or(self.default)
    }
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = r#"{
        "version": "2026-07-30",
        "default": { "in": 1.5, "out": 9, "cached_in": 0.15, "fallback": true },
        "models": {
            "gemini-3.5-flash-lite": { "in": 0.3, "out": 2.5, "cached_in": 0.03, "audio_in": 0.3 },
            "nano": { "in": 0.5, "out": 60, "out_text": 3 }
        }
    }"#;

    #[test]
    fn known_model_is_priced_exactly_and_not_a_fallback() {
        let t = PricingTable::parse(JSON).unwrap();
        assert_eq!(t.version(), "2026-07-30");
        let p = t.price("gemini-3.5-flash-lite");
        assert_eq!(p.input, 0.3);
        assert_eq!(p.output, 2.5);
        assert_eq!(p.cached, Some(0.03));
        assert_eq!(p.audio, Some(0.3));
        assert_eq!(p.output_text, None);
        assert!(!p.fallback);
    }

    #[test]
    fn image_models_carry_a_separate_text_output_rate() {
        let t = PricingTable::parse(JSON).unwrap();
        let p = t.price("nano");
        assert_eq!(p.output, 60.0);
        assert_eq!(p.output_text, Some(3.0));
    }

    #[test]
    fn unlisted_model_falls_back_to_the_conservative_default() {
        let t = PricingTable::parse(JSON).unwrap();
        let p = t.price("some-brand-new-model");
        assert_eq!(p.input, 1.5);
        assert_eq!(p.output, 9.0);
        assert!(p.fallback);
    }
}
