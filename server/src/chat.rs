// chat.rs — proxy a chat request to an LLM provider and return the whole answer,
// metered against the caller's redeemed session balance.
//
// Over the mixnet chat is NON-streaming (one reply carrying the full answer). The
// client sends messages already in OpenAI shape (`{role:"user"|"assistant", content}`)
// plus its `sessionId`. Flow: enforce the session holds credit → call the provider →
// price the usage via the pricing table + margin → charge → reply with the balance.
//
// Providers: Groq (OpenAI-compatible) and Gemini (Google's native API — the request
// and usage translation mirror src/adapters/gemini.ts + gemini-usage.ts, so the Rust
// server bills a Gemini exchange exactly like the TS server did).

use scrai_core::billing::{compute_billing, TokenUsage};
use scrai_core::pricing::PricingTable;
use serde_json::{json, Value};

/// A request that reached the provider is never free, even if it rounds to sub-1 SCRAI.
const MIN_CHARGE: u64 = 1;

/// Assumed visible-answer budget when a client sends no maxTokens of its own
/// (mirrors the TS server's SCRAI_DEFAULT_MAX_TOKENS).
fn default_max_tokens() -> u64 {
    std::env::var("SCRAI_DEFAULT_MAX_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4096)
}

/// Cap on thinking tokens (mirrors the TS server's SCRAI_THINKING_BUDGET). On Gemini
/// these bill AS OUTPUT but are NOT bounded by maxOutputTokens, so without a cap a
/// thinking model can generate far more billed output than the answer limit.
/// 0 disables thinking; raise it to trade cost for more reasoning depth.
fn thinking_budget() -> u64 {
    std::env::var("SCRAI_THINKING_BUDGET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2048)
}

/// Safe upper bound on the input tokens one attachment bills as. Gemini tiles a
/// large image into ~hundreds of tokens and a PDF page costs ~258+; this
/// over-reserves rather than risk billing above the ceiling.
const ATTACHMENT_INPUT_TOKENS: u64 = 4096;

/// Worst-case price of a request, in SCRAI — the amount to reserve. Byte length,
/// not chars/4: a byte-level BPE token decodes to at least one byte, so the byte
/// count is a GUARANTEED upper bound on the real input token count (an
/// adversarial multibyte prompt tokenises far above chars/4, and the user is
/// never charged above this ceiling, so it must not undercount).
fn ceiling_for(
    price: &scrai_core::billing::ModelPrice,
    margin: f64,
    messages: &Value,
    max_tokens: Option<u64>,
) -> u64 {
    use scrai_core::billing::{ceil_scrai, clamp_margin};
    use scrai_core::coconut::SCRAI_PER_USD;
    let retail = |usd_per_million: f64| {
        ceil_scrai(usd_per_million * SCRAI_PER_USD as f64 * clamp_margin(margin)).ceil()
    };
    let empty = Vec::new();
    let in_tokens: u64 = messages
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .map(|m| {
            let text = m.get("content").and_then(|c| c.as_str()).unwrap_or("").len() as u64;
            let atts = m.get("attachments").and_then(|a| a.as_array()).map(|a| a.len()).unwrap_or(0) as u64;
            text + atts * ATTACHMENT_INPUT_TOKENS
        })
        .sum();
    let out_tokens = max_tokens.unwrap_or_else(default_max_tokens) + thinking_budget();
    let tokens = ((in_tokens as f64 * retail(price.input) + out_tokens as f64 * retail(price.output))
        / 1_000_000.0)
        .ceil() as u64;
    // Image models bill per generated image (their providers report zero tokens);
    // one request produces at most one image, so reserve exactly one.
    tokens + per_image_scrai(price, margin)
}

/// Retail SCRAI for ONE generated image (0 for text models).
pub fn per_image_scrai(price: &scrai_core::billing::ModelPrice, margin: f64) -> u64 {
    use scrai_core::billing::{ceil_scrai, clamp_margin};
    use scrai_core::coconut::SCRAI_PER_USD;
    match price.per_image {
        Some(usd) if usd > 0.0 => {
            ceil_scrai(usd * SCRAI_PER_USD as f64 * clamp_margin(margin)).ceil() as u64
        }
        _ => 0,
    }
}

/// The price actually offered: free-TIER models bill at a reduced rate —
/// provider list price × FREE_TIER_FACTOR (default 0.5, clamped to 0..=1) —
/// because the operator's key serves them from a free daily allowance. Margin
/// still applies on top at billing time. Free and paid tiers pass unchanged.
pub fn effective_price(p: scrai_core::billing::ModelPrice) -> scrai_core::billing::ModelPrice {
    use scrai_core::billing::{ModelPrice, Tier};
    if p.tier != Tier::FreeTier {
        return p;
    }
    let f = std::env::var("FREE_TIER_FACTOR")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .map(|v| v.clamp(0.0, 1.0))
        .unwrap_or(0.5);
    ModelPrice {
        input: p.input * f,
        output: p.output * f,
        cached: p.cached.map(|c| c * f),
        audio: p.audio.map(|a| a * f),
        per_image: p.per_image.map(|i| i * f),
        ..p
    }
}

/// Handle a `chat` request envelope; returns the reply bytes (never panics).
///
/// Payment order matters and mirrors the TS server: verify the session
/// signature (it covers the canonical body WITH the small uploadId references,
/// not the megabytes) → reserve the worst-case ceiling → only then resolve
/// uploads and call the provider → settle to the real price, or refund fully on
/// a provider failure so the user pays nothing for an answer they never got.
pub async fn handle(
    request: &[u8],
    sessions: &mut scrai_core::session::SessionStore,
    uploads: &mut crate::uploads::UploadStore,
    pricing: &PricingTable,
    margin: f64,
) -> Vec<u8> {
    let v: Value = serde_json::from_slice(request).unwrap_or(Value::Null);
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    let err = |msg: &str| encode(&json!({ "id": id.clone(), "kind": "error", "error": msg }));

    let session_id = v.get("sessionId").and_then(|s| s.as_str()).unwrap_or("").to_string();
    let model = v.get("model").and_then(|m| m.as_str()).unwrap_or("").to_string();
    let messages = v.get("messages").cloned().unwrap_or_else(|| json!([]));
    let max_tokens = v.get("maxTokens").and_then(|m| m.as_u64());

    // A model with only the fallback price is never served — the operator can't
    // price it honestly, so charging for it would be guesswork on the user's dime.
    let price = effective_price(pricing.price(&model));
    if price.fallback {
        return err(&format!("model \"{model}\" has no price entry on this server"));
    }

    // Tier "free" = genuinely free forever (no provider quota behind it): no
    // signature, no session, no reserve. The request still costs the sender
    // their mixnet bandwidth credentials — that's the only price. Free-TIER
    // models do NOT come through here: they bill (reduced) like any paid model.
    if price.tier == scrai_core::billing::Tier::Free {
        let mut resolved = messages;
        if let Err(e) = uploads.resolve(&mut resolved) {
            return err(&format!("file upload failed: {e}"));
        }
        let reply = match chat(&v, resolved).await {
            Ok((text, usage, images)) => {
                let frame = compute_billing(&price, &usage, margin, 0, false);
                let usage_json = json!({
                    "inputTokens": usage.input,
                    "cachedInputTokens": usage.cached_input,
                    "audioInputTokens": usage.audio_input,
                    "outputTokens": usage.output,
                    "billing": {
                        "priceScrai": frame.price_scrai,
                        "costScrai": frame.cost_scrai,
                        "model": model,
                        "pricingVersion": pricing.version(),
                        "estimated": frame.estimated,
                        "fallbackPrice": frame.fallback_price,
                    },
                });
                let mut r = json!({ "id": id, "text": text, "usage": usage_json, "cost": 0 });
                if let Some(imgs) = images {
                    r["images"] = imgs;
                }
                r
            }
            Err(e) => json!({ "id": id, "kind": "error", "error": e }),
        };
        return encode(&reply);
    }

    // ---- payment: everything here happens BEFORE the provider is called, so a
    // request that cannot pay costs the operator nothing.
    let (Some(counter), Some(sig), Some(pem)) = (
        v.get("counter").and_then(|c| c.as_u64()),
        v.get("sig").and_then(|s| s.as_str()),
        v.get("publicKey").and_then(|p| p.as_str()),
    ) else {
        return err("this server requires a funded, signed session");
    };
    // The signature covers the canonical body, so it authorises this request and
    // no other. Field set + order must match the client exactly.
    let max_val = max_tokens.map(|m| json!(m)).unwrap_or(Value::Null);
    let body = serde_json::to_string(&json!({"model": model, "messages": messages, "maxTokens": max_val}))
        .unwrap_or_default();
    if !scrai_core::auth::session_authorises(pem, &session_id, counter, &body, sig) {
        return err("signature does not match this request");
    }

    // Reserve the worst case — this is what keeps two in-flight requests from
    // jointly overspending, and the counter check is the replay protection.
    use scrai_core::session::Reserve;
    let ceiling = ceiling_for(&price, margin, &messages, max_tokens);
    match sessions.reserve(&session_id, counter, ceiling) {
        Reserve::Ok => {}
        Reserve::Unknown => return err("unknown session — redeem coconut coins into it first"),
        Reserve::Replay { server_counter } => {
            return err(&format!(
                "counter {counter} was already used (server is at {server_counter}) — resync and retry"
            ))
        }
        Reserve::Insufficient { balance } => {
            return err(&format!(
                "not enough SCRAI: this request reserves up to {ceiling}, balance is {balance}"
            ))
        }
    }

    // Swap uploadId references for the staged image bytes (consuming them) —
    // after the signature check + reservation, so the signed body carried the
    // small references, not the megabytes.
    let mut resolved = messages;
    if let Err(e) = uploads.resolve(&mut resolved) {
        sessions.refund(&session_id, ceiling);
        return err(&format!("file upload failed: {e}"));
    }

    let reply = match chat(&v, resolved).await {
        Ok((text, usage, images)) => {
            let frame = compute_billing(&price, &usage, margin, MIN_CHARGE, false);
            // Token cost + per-image cost (image models report zero tokens).
            let n_images = images.as_ref().and_then(|i| i.as_array()).map(|a| a.len()).unwrap_or(0) as u64;
            let cost = frame.price_scrai + n_images * per_image_scrai(&price, margin);
            // Settle: the unused part of the reservation comes back.
            let balance = sessions.settle(&session_id, ceiling, cost);
            // Canonical usage the UI expects (camelCase) + a billing frame whose
            // priceScrai IS the amount charged, so footer + price + balance all agree.
            let usage_json = json!({
                "inputTokens": usage.input,
                "cachedInputTokens": usage.cached_input,
                "audioInputTokens": usage.audio_input,
                "outputTokens": usage.output,
                "billing": {
                    "priceScrai": cost,
                    "costScrai": frame.cost_scrai,
                    "model": model,
                    "pricingVersion": pricing.version(),
                    "estimated": frame.estimated,
                    "fallbackPrice": frame.fallback_price,
                },
            });
            let mut r =
                json!({ "id": id, "text": text, "usage": usage_json, "cost": cost, "balance": balance });
            if let Some(imgs) = images {
                r["images"] = imgs;
            }
            r
        }
        Err(e) => {
            // Provider failed → the user pays nothing.
            sessions.refund(&session_id, ceiling);
            json!({ "id": id, "kind": "error", "error": e })
        }
    };
    encode(&reply)
}

fn encode(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_default()
}

fn tok(usage: &Value, key: &str) -> u64 {
    usage.get(key).and_then(|t| t.as_u64()).unwrap_or(0)
}

/// The generated images of an image-model reply, as the client renders them:
/// `[{ "mimeType": …, "data": <base64> }]`. None for text models.
type Images = Option<Value>;

/// Cloudflare Workers AI image models: friendly id → Cloudflare's model path.
/// ONLY non-partner models belong here: partner models (flux-2-klein,
/// lucid-origin, phoenix-1.0, …) are billed per image in real USD, NOT covered
/// by the free daily allowance — a "free" label on those would bill the
/// operator for every request. (The adapter below also handles the SD-family
/// raw-PNG response shape, should more non-partner models be added.)
const CF_MODELS: [(&str, &str); 1] = [("flux-schnell", "@cf/black-forest-labs/flux-1-schnell")];

pub fn cf_model_path(model: &str) -> Option<&'static str> {
    CF_MODELS.iter().find(|(id, _)| *id == model).map(|(_, p)| *p)
}

/// All Cloudflare model ids, for the catalog.
pub fn cf_model_path_all() -> impl Iterator<Item = (&'static str, &'static str)> {
    CF_MODELS.iter().copied()
}

/// Call the provider for `v` (with upload refs already resolved into `messages`)
/// → (answer text, normalized token usage, generated images).
async fn chat(v: &Value, messages: Value) -> Result<(String, TokenUsage, Images), String> {
    let model = v.get("model").and_then(|m| m.as_str()).ok_or("no model")?;
    let max_tokens = v.get("maxTokens").and_then(|m| m.as_u64());

    // Route by model → provider; text catalogs are fetched live (catalog.rs),
    // image models are the static free-tier sets.
    if model.starts_with("gemini") {
        let (t, u) = gemini(model, &messages, max_tokens).await?;
        Ok((t, u, None))
    } else if model.starts_with("pollinations-") {
        pollinations(model, &messages).await
    } else if let Some(path) = cf_model_path(model) {
        cloudflare(path, &messages).await
    } else {
        let (t, u) = groq(model, messages, max_tokens).await?;
        Ok((t, u, None))
    }
}

/// The drawing prompt of an image request: all user-message text, joined.
fn prompt_of(messages: &Value) -> Result<String, String> {
    let empty = Vec::new();
    let p = messages
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string();
    if p.is_empty() {
        return Err("no prompt to draw".into());
    }
    Ok(p)
}

/// Pollinations — free, keyless image generation (see src/adapters/pollinations.ts
/// for the full privacy rationale: no key, no contract, prompt visible to a third
/// party — the mixnet still hides WHO asks). Model suffix = square size in px.
async fn pollinations(model: &str, messages: &Value) -> Result<(String, TokenUsage, Images), String> {
    use base64::Engine;
    let size: u64 = model.strip_prefix("pollinations-").and_then(|s| s.parse().ok()).unwrap_or(1024);
    let prompt = prompt_of(messages)?;
    let url = format!(
        "https://image.pollinations.ai/prompt/{}?width={size}&height={size}&nologo=true&safe=true",
        urlencoding::encode(&prompt)
    );
    // Generation is genuinely slow at larger sizes (~45s at 1536px) — and that is
    // before the mixnet gets involved, so the timeout is generous.
    let res = crate::http::client()
        .get(url)
        .timeout(std::time::Duration::from_secs(180))
        .send()
        .await
        .map_err(|e| format!("pollinations request failed: {e}"))?;
    if !res.status().is_success() {
        return Err(format!("pollinations returned {}", res.status()));
    }
    let mime = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or("image/jpeg").to_string())
        .unwrap_or_else(|| "image/jpeg".to_string());
    let bytes = res.bytes().await.map_err(|e| format!("pollinations body read failed: {e}"))?;
    if bytes.len() < 100 {
        return Err("pollinations returned an empty image".into());
    }
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok((String::new(), TokenUsage::default(), Some(json!([{ "mimeType": mime, "data": data }]))))
}

/// Cloudflare Workers AI — Flux image generation on a real free allowance
/// (10,000 Neurons/day covers the image models). Mirrors src/adapters/cloudflare.ts.
/// NOTE the envelope: Cloudflare answers 200 with success:false for model-level
/// failures, so checking the HTTP status alone would yield a silent empty image.
async fn cloudflare(path: &str, messages: &Value) -> Result<(String, TokenUsage, Images), String> {
    let token = std::env::var("CLOUDFLARE_API_TOKEN")
        .map_err(|_| "CLOUDFLARE_API_TOKEN not set".to_string())?;
    let account = std::env::var("CLOUDFLARE_ACCOUNT_ID").map_err(|_| {
        "CLOUDFLARE_ACCOUNT_ID not set — the token alone is not enough, the account id is part of the URL"
            .to_string()
    })?;
    use base64::Engine;
    let mut prompt = prompt_of(messages)?;
    prompt.truncate(2048);
    // flux takes {prompt, steps}; the SD family validates its input strictly, so
    // it gets ONLY the required {prompt} and keeps its server-side defaults.
    let body = if path.contains("flux") {
        json!({ "prompt": prompt, "steps": 4 })
    } else {
        json!({ "prompt": prompt })
    };
    let res = crate::http::client()
        .post(format!("https://api.cloudflare.com/client/v4/accounts/{account}/ai/run/{path}"))
        .bearer_auth(token)
        .json(&body)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| format!("cloudflare request failed: {e}"))?;
    let status = res.status();
    let ctype = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or("").trim().to_string())
        .unwrap_or_default();

    // SD-family models answer with the raw PNG itself, not a JSON envelope.
    if status.is_success() && ctype.starts_with("image/") {
        let bytes = res.bytes().await.map_err(|e| format!("cloudflare body read failed: {e}"))?;
        if bytes.len() < 100 {
            return Err("cloudflare returned an empty image".into());
        }
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        return Ok((String::new(), TokenUsage::default(), Some(json!([{ "mimeType": ctype, "data": data }]))));
    }

    let j: Value = res.json().await.unwrap_or(Value::Null);
    let failed = !status.is_success() || j.get("success").and_then(|s| s.as_bool()) == Some(false);
    if failed {
        let msg = j
            .pointer("/errors/0/message")
            .and_then(|m| m.as_str())
            .map(String::from)
            .unwrap_or_else(|| format!("HTTP {status}"));
        return Err(format!("cloudflare: {msg}"));
    }
    let image = j
        .pointer("/result/image")
        .and_then(|i| i.as_str())
        .ok_or("cloudflare returned no image")?;
    Ok((
        String::new(),
        TokenUsage::default(),
        Some(json!([{ "mimeType": "image/jpeg", "data": image }])),
    ))
}

// ---- Groq (OpenAI-compatible) ---------------------------------------------

async fn groq(model: &str, mut messages: Value, max_tokens: Option<u64>) -> Result<(String, TokenUsage), String> {
    let key = std::env::var("GROQ_API_KEY").map_err(|_| "GROQ_API_KEY not set".to_string())?;
    // Strip our attachments field — the OpenAI-compatible API doesn't know it
    // (Groq catalog models don't accept images anyway).
    if let Some(arr) = messages.as_array_mut() {
        for m in arr {
            if let Some(o) = m.as_object_mut() {
                o.remove("attachments");
            }
        }
    }
    let mut body = json!({ "model": model, "messages": messages });
    if let Some(mt) = max_tokens {
        body["max_tokens"] = json!(mt);
    }

    let res = crate::http::client()
        .post("https://api.groq.com/openai/v1/chat/completions")
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("groq request failed: {e}"))?;
    let status = res.status();
    let j: Value = res
        .json()
        .await
        .map_err(|e| format!("groq returned non-JSON: {e}"))?;
    if !status.is_success() {
        let msg = j
            .pointer("/error/message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        return Err(format!("groq {status}: {msg}"));
    }

    let text = j
        .pointer("/choices/0/message/content")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    let u = j.get("usage").cloned().unwrap_or(Value::Null);
    let usage = TokenUsage {
        input: tok(&u, "prompt_tokens"),
        output: tok(&u, "completion_tokens"),
        cached_input: 0,
        audio_input: 0,
    };
    Ok((text, usage))
}

// ---- Gemini (Google AI Studio, native API) --------------------------------

const GEMINI_BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";

async fn gemini(model: &str, messages: &Value, max_tokens: Option<u64>) -> Result<(String, TokenUsage), String> {
    let key = std::env::var("GEMINI_API_KEY").map_err(|_| "GEMINI_API_KEY not set".to_string())?;
    let body = to_gemini(messages, max_tokens.unwrap_or_else(default_max_tokens), thinking_budget());

    let res = crate::http::client()
        .post(format!("{GEMINI_BASE}/{model}:generateContent"))
        // Header rather than ?key= — a URL-borne secret ends up in proxy and
        // access logs; a header does not.
        .header("x-goog-api-key", key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("gemini request failed: {e}"))?;
    let status = res.status();
    let j: Value = res
        .json()
        .await
        .map_err(|e| format!("gemini returned non-JSON: {e}"))?;
    if !status.is_success() {
        let msg = j
            .pointer("/error/message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        return Err(format!("gemini {status}: {msg}"));
    }

    let text = j
        .pointer("/candidates/0/content/parts")
        .and_then(|p| p.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<String>()
        })
        .unwrap_or_default();
    let usage = gemini_usage(j.get("usageMetadata").unwrap_or(&Value::Null));
    Ok((text, usage))
}

/// Translate OpenAI-shape messages to Gemini's native request: `contents[].parts[]`
/// with roles "user"/"model" and a separate top-level `systemInstruction`.
fn to_gemini(messages: &Value, answer_tokens: u64, thinking: u64) -> Value {
    let empty = Vec::new();
    let msgs = messages.as_array().unwrap_or(&empty);
    let role_of = |m: &Value| m.get("role").and_then(|r| r.as_str()).unwrap_or("user").to_string();

    let system = msgs
        .iter()
        .filter(|m| role_of(m) == "system")
        .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
        .collect::<Vec<_>>()
        .join("\n");

    let contents: Vec<Value> = msgs
        .iter()
        .filter(|m| role_of(m) != "system")
        .map(|m| {
            // Attachments (images / PDFs / text) come first, then the text — Gemini
            // reads the parts in order.
            let mut parts: Vec<Value> = Vec::new();
            if let Some(atts) = m.get("attachments").and_then(|a| a.as_array()) {
                for att in atts {
                    let mime = att.get("mimeType").and_then(|x| x.as_str());
                    let data = att.get("data").and_then(|x| x.as_str());
                    if let (Some(mime), Some(data)) = (mime, data) {
                        parts.push(json!({ "inlineData": { "mimeType": mime, "data": data } }));
                    }
                }
            }
            match m.get("content").and_then(|c| c.as_str()) {
                Some(text) if !text.is_empty() => parts.push(json!({ "text": text })),
                _ => {}
            }
            if parts.is_empty() {
                parts.push(json!({ "text": "" }));
            }
            let role = if role_of(m) == "assistant" { "model" } else { "user" };
            json!({ "role": role, "parts": parts })
        })
        .collect();

    let mut body = json!({
        "contents": contents,
        "generationConfig": {
            // On Gemini 3.x THINKING models the thinking tokens count AGAINST
            // maxOutputTokens — a bare answer-sized budget lets thinking eat it and
            // truncates the visible answer mid-sentence. Give the answer its full
            // budget ON TOP of the thinking budget, and cap thinking so total billed
            // output stays bounded.
            "maxOutputTokens": answer_tokens + thinking,
            "thinkingConfig": { "thinkingBudget": thinking },
        },
    });
    if !system.is_empty() {
        body["systemInstruction"] = json!({ "parts": [{ "text": system }] });
    }
    body
}

/// Translate Gemini's usageMetadata into the neutral TokenUsage. Two things the
/// naive reading gets wrong, both of them money (see src/adapters/gemini-usage.ts):
///
///   1. candidatesTokenCount does NOT include thinking. Google bills output as
///      candidates + thoughts, so that sum is the output count.
///   2. promptTokenCount INCLUDES cached tokens, which bill at ~1/10th the rate.
///      Uncached input is prompt − cached (− audio, which has its own rate).
fn gemini_usage(meta: &Value) -> TokenUsage {
    let n = |key: &str| meta.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
    let prompt = n("promptTokenCount") + n("toolUsePromptTokenCount");
    let cached = n("cachedContentTokenCount").min(prompt);
    let audio = audio_tokens(meta.get("promptTokensDetails")).min(prompt - cached);
    TokenUsage {
        input: prompt - cached - audio,
        output: n("candidatesTokenCount") + n("thoughtsTokenCount"),
        cached_input: cached,
        audio_input: audio,
    }
}

fn audio_tokens(details: Option<&Value>) -> u64 {
    details
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|d| {
                    d.get("modality")
                        .and_then(|m| m.as_str())
                        .is_some_and(|m| m.eq_ignore_ascii_case("AUDIO"))
                })
                .map(|d| d.get("tokenCount").and_then(|t| t.as_u64()).unwrap_or(0))
                .sum()
        })
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemini_usage_bills_thoughts_as_output_and_splits_cached_input() {
        let meta = json!({
            "promptTokenCount": 1000,
            "cachedContentTokenCount": 400,
            "candidatesTokenCount": 200,
            "thoughtsTokenCount": 300,
            "totalTokenCount": 1500
        });
        let u = gemini_usage(&meta);
        assert_eq!(u.input, 600); // prompt minus cached
        assert_eq!(u.cached_input, 400);
        assert_eq!(u.output, 500); // candidates + thoughts — NOT candidates alone
        assert_eq!(u.audio_input, 0);
    }

    #[test]
    fn gemini_usage_counts_audio_input_separately() {
        let meta = json!({
            "promptTokenCount": 100,
            "candidatesTokenCount": 10,
            "promptTokensDetails": [
                { "modality": "TEXT", "tokenCount": 70 },
                { "modality": "AUDIO", "tokenCount": 30 }
            ]
        });
        let u = gemini_usage(&meta);
        assert_eq!(u.input, 70);
        assert_eq!(u.audio_input, 30);
    }

    #[test]
    fn gemini_usage_never_underflows_on_inconsistent_counts() {
        // A cached count larger than prompt must clamp, not wrap around u64.
        let meta = json!({ "promptTokenCount": 10, "cachedContentTokenCount": 50 });
        let u = gemini_usage(&meta);
        assert_eq!(u.input, 0);
        assert_eq!(u.cached_input, 10);
    }

    #[test]
    fn to_gemini_maps_roles_system_and_attachments() {
        let messages = json!([
            { "role": "system", "content": "be brief" },
            { "role": "user", "content": "hi", "attachments": [
                { "mimeType": "image/png", "data": "AAAA" }
            ]},
            { "role": "assistant", "content": "hello" }
        ]);
        let body = to_gemini(&messages, 4096, 2048);

        assert_eq!(body.pointer("/systemInstruction/parts/0/text").unwrap(), "be brief");
        // attachment part precedes the text part
        assert_eq!(body.pointer("/contents/0/role").unwrap(), "user");
        assert_eq!(body.pointer("/contents/0/parts/0/inlineData/mimeType").unwrap(), "image/png");
        assert_eq!(body.pointer("/contents/0/parts/1/text").unwrap(), "hi");
        assert_eq!(body.pointer("/contents/1/role").unwrap(), "model");
        // answer budget sits ON TOP of the thinking budget, and thinking is capped
        assert_eq!(body.pointer("/generationConfig/maxOutputTokens").unwrap(), 4096 + 2048);
        assert_eq!(body.pointer("/generationConfig/thinkingConfig/thinkingBudget").unwrap(), 2048);
    }

    #[test]
    fn to_gemini_omits_system_instruction_when_there_is_none() {
        let body = to_gemini(&json!([{ "role": "user", "content": "hi" }]), 100, 0);
        assert!(body.get("systemInstruction").is_none());
        assert_eq!(body.pointer("/contents/0/parts/0/text").unwrap(), "hi");
    }

    // ---- payment path: signature → reserve → refund -------------------------

    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    use ed25519_dalek::{Signer, SigningKey};
    use scrai_core::auth;

    fn session_keypair() -> (SigningKey, String, String) {
        let sk = SigningKey::from_bytes(&[3u8; 32]);
        let mut der = vec![0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];
        der.extend_from_slice(&sk.verifying_key().to_bytes());
        let pem = format!("-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n", B64.encode(der));
        let sid = auth::id_for(&pem);
        (sk, pem, sid)
    }

    /// Build a chat envelope the way the CLIENT does (same json! field order,
    /// same canonical-body construction) — this is the byte-compat contract.
    fn signed_chat(sk: &SigningKey, pem: &str, sid: &str, counter: u64, model: &str) -> Vec<u8> {
        let messages = json!([{ "role": "user", "content": "hi" }]);
        let body = serde_json::to_string(
            &json!({"model": model, "messages": messages, "maxTokens": json!(64)}),
        )
        .unwrap();
        let body_hash = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(body.as_bytes()));
        let sig = B64.encode(sk.sign(format!("{sid}:{counter}:{body_hash}").as_bytes()).to_bytes());
        json!({"kind":"chat","id":"t1","model":model,"messages":messages,"maxTokens":64,
            "sessionId":sid,"counter":counter,"sig":sig,"publicKey":pem})
        .to_string()
        .into_bytes()
    }

    #[tokio::test]
    async fn chat_verifies_signature_reserves_and_refunds_on_provider_failure() {
        let (sk, pem, sid) = session_keypair();
        let mut sessions = scrai_core::session::SessionStore::default();
        let mut uploads = crate::uploads::UploadStore::default();
        let pricing = PricingTable::parse(
            r#"{"version":"t","default":{"in":1.0,"out":4.0,"fallback":true},
                "models":{"m":{"in":1.0,"out":4.0},"m2":{"in":1.0,"out":4.0}}}"#,
        )
        .unwrap();
        sessions.credit(&sid, 100_000);

        // Unsigned → refused before anything happens.
        let bare = json!({"kind":"chat","id":"x","model":"m","messages":[]}).to_string();
        let r: Value = serde_json::from_slice(
            &handle(bare.as_bytes(), &mut sessions, &mut uploads, &pricing, 1.4).await,
        )
        .unwrap();
        assert!(r["error"].as_str().unwrap().contains("signed session"));

        // Tampered model (signature covers the body) → refused, balance untouched.
        // (Tampers to another PRICED model — an unpriced one is rejected by the
        // price check before the signature is even looked at.)
        let mut env: Value = serde_json::from_slice(&signed_chat(&sk, &pem, &sid, 1, "m")).unwrap();
        env["model"] = json!("m2");
        let r: Value = serde_json::from_slice(
            &handle(env.to_string().as_bytes(), &mut sessions, &mut uploads, &pricing, 1.4).await,
        )
        .unwrap();
        assert!(r["error"].as_str().unwrap().contains("signature"));
        assert_eq!(sessions.balance(&sid), 100_000);

        // Valid signature: reserve happens, provider fails (no GROQ_API_KEY for
        // model "m" in the test env) → FULL refund, but the counter is consumed.
        let r: Value = serde_json::from_slice(
            &handle(&signed_chat(&sk, &pem, &sid, 1, "m"), &mut sessions, &mut uploads, &pricing, 1.4).await,
        )
        .unwrap();
        assert!(r.get("error").is_some());
        assert_eq!(sessions.balance(&sid), 100_000); // refunded
        assert_eq!(sessions.status(&sid).1, 1); // counter advanced

        // Replaying the same counter is now refused.
        let r: Value = serde_json::from_slice(
            &handle(&signed_chat(&sk, &pem, &sid, 1, "m"), &mut sessions, &mut uploads, &pricing, 1.4).await,
        )
        .unwrap();
        assert!(r["error"].as_str().unwrap().contains("already used"));
    }

    #[test]
    fn free_tier_models_bill_at_the_reduced_rate_and_paid_models_do_not() {
        let pricing = PricingTable::parse(
            r#"{"version":"t","default":{"in":1.5,"out":9.0,"fallback":true},
                "models":{
                  "ft":{"in":1.0,"out":4.0,"tier":"free-tier","per_image":0.001},
                  "pd":{"in":1.0,"out":4.0}
                }}"#,
        )
        .unwrap();
        let ft = effective_price(pricing.price("ft"));
        assert_eq!(ft.input, 0.5); // default FREE_TIER_FACTOR = 0.5
        assert_eq!(ft.output, 2.0);
        assert_eq!(ft.per_image, Some(0.0005));
        let pd = effective_price(pricing.price("pd"));
        assert_eq!(pd.input, 1.0); // paid: untouched
        assert_eq!(pd.output, 4.0);
        // Per-image retail: 0.0005 USD × 100_000 SCRAI/USD × margin 1.4 = 70 SCRAI.
        assert_eq!(per_image_scrai(&ft, 1.4), 70);
        assert_eq!(per_image_scrai(&pd, 1.4), 0);
    }

    #[tokio::test]
    async fn unpriced_models_are_rejected_and_free_models_skip_the_paywall() {
        let mut sessions = scrai_core::session::SessionStore::default();
        let mut uploads = crate::uploads::UploadStore::default();
        let pricing = PricingTable::parse(
            r#"{"version":"t","default":{"in":1.0,"out":4.0,"fallback":true},
                "models":{"pollinations-512":{"in":0.0,"out":0.0,"tier":"free"}}}"#,
        )
        .unwrap();

        // Fallback-priced model → refused outright, even before the auth check.
        let req = json!({"kind":"chat","id":"x","model":"mystery","messages":[]}).to_string();
        let r: Value = serde_json::from_slice(
            &handle(req.as_bytes(), &mut sessions, &mut uploads, &pricing, 1.4).await,
        )
        .unwrap();
        assert!(r["error"].as_str().unwrap().contains("no price entry"));

        // Explicit 0/0 model → passes the paywall with NO signature or session at
        // all. It reaches the provider adapter, which fails on the empty prompt —
        // proving the request got past every payment gate.
        let req = json!({"kind":"chat","id":"x","model":"pollinations-512","messages":[]}).to_string();
        let r: Value = serde_json::from_slice(
            &handle(req.as_bytes(), &mut sessions, &mut uploads, &pricing, 1.4).await,
        )
        .unwrap();
        assert!(r["error"].as_str().unwrap().contains("no prompt to draw"));
    }
}
