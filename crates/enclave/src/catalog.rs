//! The model catalogue the app shows: what is offered (`policy::OFFERED_MODELS`), from a
//! provider this enclave has a key for, at the retail rates it will charge. Built from the
//! attested policy and prices alone, so what the picker says is what the bill does.

use crate::policy;
use crate::provider::Providers;
use serde_json::{json, Value};
use tokumai_core::billing::{ceil_toku, TOKU_PER_USD, IMAGE_SIZES};
use tokumai_core::pricing::PricingTable;

/// Retail TOKU per 1M tokens.
fn retail(usd_per_million: f64) -> u64 {
    ceil_toku(usd_per_million * TOKU_PER_USD as f64 * policy::MARGIN).ceil() as u64
}

/// Whether `model` may be asked at all.
pub fn offered(model: &str, pricing: &PricingTable, dev_mode: bool) -> bool {
    (policy::OFFERED_MODELS.contains(&model) && !pricing.price(model).fallback) || (dev_mode && model == "mock")
}

pub fn models(pricing: &PricingTable, providers: &Providers, dev_mode: bool) -> Vec<Value> {
    let mut ids: Vec<&str> = policy::OFFERED_MODELS.to_vec();
    if dev_mode {
        ids.insert(0, "mock");
    }
    ids.into_iter()
        .filter(|m| offered(m, pricing, dev_mode))
        .filter_map(|m| providers.find(m).map(|p| (m, p.name())))
        .map(|(m, provider)| {
            let price = pricing.price(m);
            let image = crate::gemini::is_image_model(m);
            let mut rate = json!({ "in": retail(price.input), "out": retail(price.output) });
            if let Some(t) = price.output_text {
                rate["outText"] = json!(retail(t));
            }
            if image {
                let per = |tokens: u64| (retail(price.output) as f64 * tokens as f64 / 1_000_000.0).ceil() as u64;
                let supported = crate::gemini::supported_image_sizes(m);
                if crate::gemini::model_takes_image_size(m) {
                    let sizes: serde_json::Map<String, Value> =
                        IMAGE_SIZES.iter().filter(|(s, _)| supported.contains(s)).map(|(s, t)| (s.to_string(), json!(per(*t)))).collect();
                    rate["imageSizes"] = Value::Object(sizes);
                }
                // The picker's per-picture figure: 1K, or the model's fixed size.
                let tokens = if m.starts_with("gemini-2.5") { 1290 } else { 1120 };
                rate["image"] = json!(per(tokens));
            }
            let (vendor, openai) = match provider {
                "openai" => ("OpenAI", true),
                "gemini" => ("Google", false),
                _ => ("tokumai", false),
            };
            let mut entry = json!({
                "model": m,
                "label": pricing.label(m).unwrap_or(m),
                "vendor": vendor,
                "kind": if image { "image" } else { "text" },
                "rate": rate,
                "tier": price.tier.as_str(),
                // Paid API use: neither provider trains on it.
                "trainsOnInput": false,
                "acceptsImages": true,
                "live": !image,
            });
            if openai {
                entry["retentionDays"] = json!(policy::OPENAI_RETENTION_DAYS);
                entry["timeoutMs"] = json!(180_000);
            }
            entry
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_offered_model_is_priced_and_the_catalogue_follows_the_providers() {
        let pricing = PricingTable::parse(policy::PRICING_JSON).unwrap();
        for m in policy::OFFERED_MODELS {
            assert!(!pricing.price(m).fallback, "{m} is offered but not priced");
        }
        assert!(!offered("gemini-flash-latest", &pricing, false), "a floating alias is not offered");
        assert!(!offered("mock", &pricing, false));
        // Only the mock provider here: the catalogue holds the mock and nothing else.
        let list = models(&pricing, &Providers::mock(), true);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["model"], "mock");
        assert!(models(&pricing, &Providers::mock(), false).is_empty());
    }
}
