// catalog.rs — the model catalog, fetched LIVE from the providers so it never goes
// stale (Groq deprecates model ids regularly; Google rotates Gemini previews).
// Providers fail independently: a missing key or an unreachable API just drops that
// provider's models from the reply instead of emptying the whole catalog.
//
// Each model's retail rate comes from the pricing table (USD/1M × peg × margin), so
// the price shown in the picker is exactly what chat.rs will charge. Unlisted Groq
// models get the conservative fallback price; unpriced Gemini models are dropped
// (the live list is a zoo of previews we'd otherwise show at a surprise price).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use scrai_core::billing::ceil_scrai;
use scrai_core::coconut::SCRAI_PER_USD;
use scrai_core::pricing::PricingTable;
use serde_json::{json, Value};

/// The catalog changes rarely (provider model lists rotate over hours/days), but an
/// anonymous caller can spam `models` and turn each request into two upstream GETs
/// (M-srv-1 amplification). Cache the assembled list process-wide for this long and
/// serve repeats from memory. `pricing`/`margin` are fixed per process, so the cached
/// list is valid for every caller; only the reply `id` is stitched in per request.
const CATALOG_TTL: Duration = Duration::from_secs(60);
static CATALOG_CACHE: Mutex<Option<(Instant, Vec<Value>)>> = Mutex::new(None);

/// Provider allowlist: SCRAI_PROVIDERS="gemini" (comma list) limits the
/// catalog to those providers; unset/empty/"all" offers everything available.
/// Trims the PICKER only — pricing still guards direct requests in chat.rs.
pub fn provider_enabled(name: &str) -> bool {
    match std::env::var("SCRAI_PROVIDERS") {
        Err(_) => true,
        Ok(v) => {
            let v = v.trim().to_lowercase();
            v.is_empty() || v == "all" || v.split(',').any(|p| p.trim() == name)
        }
    }
}

pub async fn handle(request: &[u8], pricing: &PricingTable, margin: f64) -> Vec<u8> {
    let v: Value = serde_json::from_slice(request).unwrap_or(Value::Null);
    let id = v.get("id").cloned().unwrap_or(Value::Null);

    // Serve a fresh cached list without touching the providers. Never hold the lock
    // across the await below.
    if let Some(models) = cached_fresh() {
        return serde_json::to_vec(&json!({ "id": id, "models": models, "testnet": crate::pay::is_testnet_server(), "faucetUrl": crate::pay::faucet_url(), "card": crate::pay::card_info() })).unwrap_or_default();
    }

    let models = fetch_models(pricing, margin).await;
    if let Ok(mut guard) = CATALOG_CACHE.lock() {
        *guard = Some((Instant::now(), models.clone()));
    }
    serde_json::to_vec(&json!({ "id": id, "models": models, "testnet": crate::pay::is_testnet_server(), "faucetUrl": crate::pay::faucet_url(), "card": crate::pay::card_info() })).unwrap_or_default()
}

/// A clone of the cached model list if it exists and is within its TTL, else `None`.
fn cached_fresh() -> Option<Vec<Value>> {
    let guard = CATALOG_CACHE.lock().ok()?;
    let (at, models) = guard.as_ref()?;
    (at.elapsed() < CATALOG_TTL).then(|| models.clone())
}

/// Assemble the live catalog from the providers (the uncached path).
async fn fetch_models(pricing: &PricingTable, margin: f64) -> Vec<Value> {
    // Gemini first — its models lead the picker (same provider order as the TS server).
    let (gemini, groq) = tokio::join!(
        async {
            if provider_enabled("gemini") { gemini_models(pricing, margin).await } else { Ok(Vec::new()) }
        },
        async {
            if provider_enabled("groq") { groq_models(pricing, margin).await } else { Ok(Vec::new()) }
        }
    );
    let mut models = Vec::new();
    for (provider, result) in [("gemini", gemini), ("groq", groq)] {
        match result {
            Ok(mut m) => models.append(&mut m),
            Err(e) => eprintln!("scrai-server: {provider} catalog fetch failed: {e}"),
        }
    }
    models.append(&mut image_models(pricing, margin));
    models
}

/// Retail rate in SCRAI per 1M tokens (provider USD price × peg × margin).
fn retail(usd_per_million: f64, margin: f64) -> u64 {
    ceil_scrai(usd_per_million * SCRAI_PER_USD as f64 * margin).ceil() as u64
}

/// The static free-tier image models: pollinations is keyless (always offered),
/// Cloudflare needs its token + account id. All are explicitly priced 0/0, so
/// chat.rs serves them without a funded session.
fn image_models(pricing: &PricingTable, margin: f64) -> Vec<Value> {
    let mut out = Vec::new();
    let mut push = |id: &str, vendor: &str, trains: bool| {
        let price = crate::chat::effective_price(pricing.price(id));
        if price.fallback {
            return; // an unpriced image model would be rejected by chat.rs anyway
        }
        let mut rate = json!({ "in": retail(price.input, margin), "out": retail(price.output, margin) });
        // Image models price per generated image, not per token.
        let img = crate::chat::per_image_scrai(&price, margin);
        if img > 0 {
            rate["image"] = json!(img);
        }
        out.push(json!({
            "model": id,
            "label": pricing.label(id).unwrap_or(id),
            "vendor": vendor,
            "kind": "image",
            "rate": rate,
            "tier": price.tier.as_str(),
            "trainsOnInput": trains,
            "acceptsImages": false,
        }));
    };
    let cf_ready = std::env::var("CLOUDFLARE_API_TOKEN").is_ok_and(|v| !v.trim().is_empty())
        && std::env::var("CLOUDFLARE_ACCOUNT_ID").is_ok_and(|v| !v.trim().is_empty());
    if cf_ready && provider_enabled("cloudflare") {
        for (id, _) in crate::chat::cf_model_path_all() {
            push(id, "Cloudflare", false);
        }
    }
    // Public, keyless, no contract — the prompt is visible to the operator, so
    // it carries the "trains on input" badge (assume the worst).
    if provider_enabled("pollinations") {
        for id in ["pollinations-512", "pollinations-1024", "pollinations-1536"] {
            push(id, "Pollinations", true);
        }
    }
    // Google (Gemini) native image generation — Nano Banana. Bills by TOKENS: the image
    // itself is a fixed count of IMAGE output tokens at the `out` rate (1K: 1290 on Nano
    // Banana, 1120 on Nano Banana 2 / 2 Lite), the model's text + thinking bill at
    // `out_text` — `chat.rs`'s Gemini accounting charges exactly that split. We surface
    // a per-image estimate (1K, image tokens only) for the picker on top of the token
    // rates. Only shows on a real (non-fallback) price; paid tier → no training badge.
    if provider_enabled("gemini") {
        let trains = crate::chat::gemini_trains_on_input();
        for (id, img_tokens) in [
            ("gemini-2.5-flash-image", 1290.0),
            ("gemini-3.1-flash-image", 1120.0),
            ("gemini-3.1-flash-lite-image", 1120.0),
        ] {
            let price = crate::chat::effective_price(pricing.price(id));
            if price.fallback {
                continue;
            }
            let mut rate = json!({ "in": retail(price.input, margin), "out": retail(price.output, margin) });
            if let Some(t) = price.output_text {
                rate["outText"] = json!(retail(t, margin));
            }
            let per_tokens = |tokens: f64| (retail(price.output, margin) as f64 * tokens / 1_000_000.0).ceil() as u64;
            let per_img = per_tokens(img_tokens);
            if per_img > 0 {
                rate["image"] = json!(per_img);
            }
            // Selectable sizes with their per-picture retail price (image tokens only —
            // the model's text/thinking is a small extra at `outText`).
            if crate::chat::model_takes_image_size(id) {
                let supported = crate::chat::supported_image_sizes(id);
                let sizes: serde_json::Map<String, Value> = crate::chat::image_sizes()
                    .iter()
                    .filter(|(s, _)| supported.contains(s))
                    .map(|(s, t)| (s.to_string(), json!(per_tokens(*t as f64))))
                    .collect();
                rate["imageSizes"] = Value::Object(sizes);
            }
            out.push(json!({
                "model": id,
                "label": pricing.label(id).unwrap_or(id),
                "vendor": "Google",
                "kind": "image",
                "rate": rate,
                "tier": price.tier.as_str(),
                "trainsOnInput": trains,
                // Nano Banana is multimodal-in: it can edit a supplied image, so attachments help.
                "acceptsImages": true,
            }));
        }
    }
    out
}

async fn groq_models(pricing: &PricingTable, margin: f64) -> Result<Vec<Value>, String> {
    let key = std::env::var("GROQ_API_KEY").map_err(|_| "GROQ_API_KEY not set".to_string())?;
    let res = crate::http::client()
        .get("https://api.groq.com/openai/v1/models")
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| format!("groq models request failed: {e}"))?;
    let status = res.status();
    let body = res
        .text()
        .await
        .map_err(|e| format!("groq models body read failed: {e}"))?;
    let j: Value = serde_json::from_str(&body)
        .map_err(|e| format!("groq models non-JSON ({status}): {e}"))?;
    let data = j.get("data").and_then(|d| d.as_array()).ok_or_else(|| {
        let detail = j
            .get("error")
            .map(|e| e.to_string())
            .unwrap_or_else(|| body.chars().take(300).collect());
        format!("groq models ({status}): {detail}")
    })?;

    let mut out = Vec::new();
    for m in data {
        let Some(id) = m.get("id").and_then(|i| i.as_str()) else {
            continue;
        };
        // Skip non-chat models (audio / safety / embeddings).
        let low = id.to_lowercase();
        if ["whisper", "tts", "guard", "embed"].iter().any(|k| low.contains(k)) {
            continue;
        }
        let price = crate::chat::effective_price(pricing.price(id));
        out.push(json!({
            "model": id,
            "label": pricing.label(id).unwrap_or(id),
            "vendor": "Groq",
            "kind": "text",
            "rate": { "in": retail(price.input, margin), "out": retail(price.output, margin) },
            "tier": price.tier.as_str(),
            "trainsOnInput": false,
            "acceptsImages": false,
        }));
    }
    Ok(out)
}

async fn gemini_models(pricing: &PricingTable, margin: f64) -> Result<Vec<Value>, String> {
    let (key, _) = crate::chat::gemini_api_key()?;
    let res = crate::http::client()
        .get("https://generativelanguage.googleapis.com/v1beta/models?pageSize=200")
        .header("x-goog-api-key", key)
        .send()
        .await
        .map_err(|e| format!("gemini models request failed: {e}"))?;
    let status = res.status();
    let body = res
        .text()
        .await
        .map_err(|e| format!("gemini models body read failed: {e}"))?;
    let j: Value = serde_json::from_str(&body)
        .map_err(|e| format!("gemini models non-JSON ({status}): {e}"))?;
    let data = j.get("models").and_then(|d| d.as_array()).ok_or_else(|| {
        // Google signals key/region problems as 200-shaped JSON with an `error`
        // object — surface its message instead of a bare "no models array".
        let detail = j
            .get("error")
            .map(|e| e.to_string())
            .unwrap_or_else(|| body.chars().take(300).collect());
        format!("gemini models ({status}): {detail}")
    })?;

    // Text/multimodal chat models only, mirroring the TS adapter's discover():
    // generateContent-capable `gemini-*` ids, minus embeddings/media/specialty models
    // (image OUTPUT stays out until the image path is ported).
    const EXCLUDE: [&str; 11] = [
        "embedding", "aqa", "image", "imagen", "tts", "native-audio", "live", "veo",
        "robotics", "computer-use", "video-understanding",
    ];

    let mut out = Vec::new();
    for m in data {
        let methods = m.get("supportedGenerationMethods").and_then(|s| s.as_array());
        let generates = methods
            .is_some_and(|ms| ms.iter().any(|x| x.as_str() == Some("generateContent")));
        if !generates {
            continue;
        }
        let Some(id) = m
            .get("name")
            .and_then(|n| n.as_str())
            .map(|n| n.trim_start_matches("models/"))
        else {
            continue;
        };
        if !id.starts_with("gemini-") {
            continue;
        }
        let low = id.to_lowercase();
        if EXCLUDE.iter().any(|k| low.contains(k)) {
            continue;
        }
        // Explicitly priced models only: an unlisted Gemini model would bill at the
        // conservative default — a paid model we can't price honestly in the picker.
        let price = crate::chat::effective_price(pricing.price(id));
        if price.fallback {
            continue;
        }
        out.push(json!({
            "model": id,
            "label": pricing.label(id).unwrap_or(id),
            "vendor": "Google",
            "kind": "text",
            "rate": { "in": retail(price.input, margin), "out": retail(price.output, margin) },
            "tier": price.tier.as_str(),
            // Paid (mainnet) Gemini does NOT train on prompts; free/testnet does — surfaced honestly.
            "trainsOnInput": crate::chat::gemini_trains_on_input(),
            // Gemini reads images, PDFs and text via inlineData attachments.
            "acceptsImages": true,
        }));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    /// Live smoke test against the real provider APIs (needs .env keys + network).
    /// Run explicitly: cargo test -p scrai-server live_catalog -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_catalog_lists_both_providers() {
        dotenvy::dotenv().ok();
        let pricing = PricingTable::parse(include_str!("../../pricing.json")).unwrap();
        match gemini_models(&pricing, 1.4).await {
            Ok(m) => println!("gemini: {} models: {:?}", m.len(),
                m.iter().filter_map(|x| x.get("model").and_then(|v| v.as_str())).collect::<Vec<_>>()),
            Err(e) => println!("gemini FAILED: {e}"),
        }
        match groq_models(&pricing, 1.4).await {
            Ok(m) => println!("groq: {} models", m.len()),
            Err(e) => println!("groq FAILED: {e}"),
        }
    }
}
