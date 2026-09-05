// catalog.rs — the model catalog, fetched LIVE from the providers so it never goes
// stale (Google rotates Gemini previews constantly).
// Providers fail independently: a missing key or an unreachable API just drops that
// provider's models from the reply instead of emptying the whole catalog.
//
// Each model's retail rate comes from the pricing table (USD/1M × peg × margin), so
// the price shown in the picker is exactly what chat.rs will charge. Unpriced Gemini
// models are dropped (the live list is a zoo of previews we'd otherwise show at a
// surprise price); OpenAI is an explicit allowlist with no live listing at all.

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

/// Provider allowlist: PROVIDERS="gemini" (comma list) limits the
/// catalog to those providers; unset/empty/"all" offers everything available.
/// Enforced for the picker here AND for every chat via `model_offered` below.
pub fn provider_enabled(name: &str) -> bool {
    match crate::cfg("PROVIDERS") {
        Err(_) => true,
        Ok(v) => provider_in_list(name, &v),
    }
}

/// The pure half of `provider_enabled`, so the admission rules can be tested without
/// writing to process-global env (which every other test in this crate would see).
fn provider_in_list(name: &str, list: &str) -> bool {
    let v = list.trim().to_lowercase();
    v.is_empty() || v == "all" || v.split(',').any(|p| p.trim() == name)
}

/// Which provider serves `model`. Deliberately the SAME routing `chat::chat` uses to
/// pick an adapter, in the same order, so the admission decision below and the call
/// that actually happens can never disagree about who gets the request.
pub fn provider_of_model(model: &str) -> &'static str {
    if model.starts_with("gemini") {
        "gemini"
    } else if crate::openai::is_openai_model(model) {
        "openai"
    } else {
        // Not routable by chat.rs either — `model_offered` therefore says no, and an
        // unknown id can never fall through to some unnamed provider.
        "unknown"
    }
}

/// Does this server OFFER `model` right now?
///
/// The picker is built from this decision (`fetch_models`) and `chat::reserve` enforces
/// the SAME one before anything is reserved or sent. That symmetry is the point: until
/// 2026-09-04 the only server-side guard on a chat was "does pricing.json price this
/// model", so every rule that lived in the catalog was a client-side suggestion. A
/// hand-written request naming `gpt-5.4` was served on a TESTNET server — where the
/// catalog offers nano only, precisely because testers pay in faucet dollars while
/// OpenAI bills us real ones — and likewise reached any provider the operator had
/// switched off with PROVIDERS while its key was still in the env.
///
/// Rule of thumb for anything added here later: a restriction that only shapes the
/// catalog is a UI hint. If it protects money, an API key or a policy, it has to be
/// asked again on the path that does the work.
pub fn model_offered(model: &str) -> bool {
    offered_with(model, &crate::cfg("PROVIDERS").unwrap_or_default(), &openai_model_ids())
}

/// The decision itself, with the two config values passed in — pure, so the test below
/// does not have to mutate env that the rest of this crate's tests read.
fn offered_with(model: &str, providers: &str, openai_ids: &[String]) -> bool {
    let provider = provider_of_model(model);
    // Fail closed on anything chat.rs cannot route. An empty PROVIDERS means "every
    // provider we HAVE", never "any name a caller invents" — the removed test providers
    // land here, and so would a typo'd id that pricing.json happens to price.
    if provider == "unknown" {
        return false;
    }
    if !provider_in_list(provider, providers) {
        return false;
    }
    // OpenAI has no live listing — an explicit allowlist (`OPENAI_MODELS`, or the
    // built-in set, narrowed to the cheapest model on a testnet server).
    if provider == "openai" && !openai_ids.iter().any(|id| id == model) {
        return false;
    }
    true
}

/// `identities`: every Nym address this server answers on (its multi-identity front
/// doors). The app keeps them as fallbacks — same server, same money, other gateway.
pub async fn handle(request: &[u8], pricing: &PricingTable, margin: f64, identities: &[String]) -> Vec<u8> {
    let v: Value = serde_json::from_slice(request).unwrap_or(Value::Null);
    let id = v.get("id").cloned().unwrap_or(Value::Null);

    // Serve a fresh cached list without touching the providers. Never hold the lock
    // across the await below.
    if let Some(models) = cached_fresh() {
        return serde_json::to_vec(&json!({ "id": id, "models": models, "testnet": crate::pay::is_testnet_server(), "faucetUrl": crate::pay::faucet_url(), "siteUrl": crate::pay::site_url(), "card": crate::pay::card_info().await, "serverVersion": crate::VERSION, "identities": identities })).unwrap_or_default();
    }

    let models = fetch_models(pricing, margin).await;
    if let Ok(mut guard) = CATALOG_CACHE.lock() {
        *guard = Some((Instant::now(), models.clone()));
    }
    serde_json::to_vec(&json!({ "id": id, "models": models, "testnet": crate::pay::is_testnet_server(), "faucetUrl": crate::pay::faucet_url(), "siteUrl": crate::pay::site_url(), "card": crate::pay::card_info().await, "serverVersion": crate::VERSION, "identities": identities })).unwrap_or_default()
}

/// A clone of the cached model list if it exists and is within its TTL, else `None`.
fn cached_fresh() -> Option<Vec<Value>> {
    let guard = CATALOG_CACHE.lock().ok()?;
    let (at, models) = guard.as_ref()?;
    (at.elapsed() < CATALOG_TTL).then(|| models.clone())
}

/// Assemble the live catalog from the providers (the uncached path).
async fn fetch_models(pricing: &PricingTable, margin: f64) -> Vec<Value> {
    // Gemini leads the picker. OpenAI needs no provider round trip: we offer an
    // explicit allowlist ∩ pricing.json.
    let openai = if provider_enabled("openai") { openai_models(pricing, margin) } else { Ok(Vec::new()) };
    let gemini = if provider_enabled("gemini") { gemini_models(pricing, margin).await } else { Ok(Vec::new()) };
    let mut models = Vec::new();
    for (provider, result) in [("gemini", gemini), ("openai", openai)] {
        match result {
            Ok(mut m) => models.append(&mut m),
            Err(e) => eprintln!("scrai-server: {provider} catalog fetch failed: {e}"),
        }
    }
    models.append(&mut image_models(pricing, margin));
    models
}

/// Retail rate in TOKU per 1M tokens (provider USD price × peg × margin).
fn retail(usd_per_million: f64, margin: f64) -> u64 {
    ceil_scrai(usd_per_million * SCRAI_PER_USD as f64 * margin).ceil() as u64
}

/// The image models, all Google's — billed by tokens like any other Gemini call.
fn image_models(pricing: &PricingTable, margin: f64) -> Vec<Value> {
    let mut out = Vec::new();
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

/// OpenAI text/reasoning models we offer. No live listing (`/v1/models` mixes in
/// embeddings, TTS, fine-tunes): an explicit allowlist, each only when pricing.json prices
/// it (an unpriced model is never offered). Verified ids/prices: 2026-09-03.
const OPENAI_MODELS: [&str; 3] = ["gpt-5.4-nano", "gpt-5.4-mini", "gpt-5.4"];

/// The ids actually offered: OPENAI_MODELS (comma list) overrides; otherwise the
/// full allowlist — except on a TESTNET server, where testers pay with faucet dollars
/// while OpenAI bills us real ones, so only the cheapest model is offered there.
fn openai_model_ids() -> Vec<String> {
    if let Ok(v) = crate::cfg("OPENAI_MODELS") {
        let ids: Vec<String> = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        if !ids.is_empty() {
            return ids;
        }
    }
    if crate::pay::is_testnet_server() {
        return vec!["gpt-5.4-nano".to_string()];
    }
    OPENAI_MODELS.iter().map(|s| s.to_string()).collect()
}

fn openai_models(pricing: &PricingTable, margin: f64) -> Result<Vec<Value>, String> {
    let _ = crate::openai::api_key()?; // no key → the provider is simply absent
    let mut out = Vec::new();
    for id in openai_model_ids() {
        let id = id.as_str();
        let price = crate::chat::effective_price(pricing.price(id));
        if price.fallback {
            continue;
        }
        out.push(json!({
            "model": id,
            "label": pricing.label(id).unwrap_or(id),
            "vendor": "OpenAI",
            "kind": "text",
            "rate": { "in": retail(price.input, margin), "out": retail(price.output, margin) },
            "tier": price.tier.as_str(),
            // API inputs are not used for training (business terms) …
            "trainsOnInput": false,
            // … but retained for abuse monitoring: 30 days, or 0 with Zero Data Retention.
            "retentionDays": crate::openai::retention_days(),
            // Reasoning answers can take a minute or two — the app waits this long.
            "timeoutMs": crate::openai::TIMEOUT_MS,
            "acceptsImages": true,
            // The Responses `web_search` tool, attached when the app sends `live`.
            "live": true,
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
            // Text models can ground answers with a web search when the app sends `live`.
            "live": true,
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
    async fn live_catalog_lists_the_providers() {
        dotenvy::dotenv().ok();
        let pricing = PricingTable::parse(include_str!("../../pricing.json")).unwrap();
        match gemini_models(&pricing, 1.4).await {
            Ok(m) => println!("gemini: {} models: {:?}", m.len(),
                m.iter().filter_map(|x| x.get("model").and_then(|v| v.as_str())).collect::<Vec<_>>()),
            Err(e) => println!("gemini FAILED: {e}"),
        }
        match openai_models(&pricing, 1.4) {
            Ok(m) => println!("openai: {} models", m.len()),
            Err(e) => println!("openai FAILED: {e}"),
        }
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// The catalog's rules must hold on the CHAT path too — `model_offered` is what
    /// `chat::reserve` asks. Driven through the pure `offered_with`, so this test never
    /// touches process-global env (which the chat/pay tests in this crate read).
    #[test]
    fn offered_follows_the_provider_allowlist_and_the_openai_id_list() {
        let all = ids(&["gpt-5.4-nano", "gpt-5.4-mini", "gpt-5.4"]);
        // routing matches chat::chat's dispatch order
        assert_eq!(provider_of_model("gemini-3.5-flash"), "gemini");
        assert_eq!(provider_of_model("gpt-5.4"), "openai");
        assert_eq!(provider_of_model("llama-3.3-70b-versatile"), "unknown");
        assert_eq!(provider_of_model("pollinations-512"), "unknown");

        // no allowlist → everything priced is offered
        assert!(offered_with("gemini-3.5-flash", "", &all));
        assert!(offered_with("gpt-5.4", "", &all));
        // a provider we no longer route is never offered, allowlist or not
        assert!(!offered_with("pollinations-512", "", &all));
        assert!(!offered_with("llama-3.3-70b-versatile", "", &all));
        assert!(offered_with("gpt-5.4", "all", &all));

        // a provider the operator switched off is refused even when named directly …
        assert!(offered_with("gemini-3.5-flash", "gemini", &all));
        assert!(!offered_with("gpt-5.4-nano", "gemini", &all), "openai is off");

        // the OpenAI id list is an allowlist, not a hint — this is the testnet rule
        // (nano only, because testers pay in faucet dollars and OpenAI bills us real ones)
        let nano = ids(&["gpt-5.4-nano"]);
        assert!(offered_with("gpt-5.4-nano", "", &nano));
        assert!(!offered_with("gpt-5.4", "", &nano), "an id outside the list must not be served");
        assert!(!offered_with("gpt-5.4-mini", "", &nano));
    }
}
