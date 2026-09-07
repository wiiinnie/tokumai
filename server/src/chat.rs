// chat.rs — proxy a chat request to an LLM provider and return the whole answer,
// metered against the caller's redeemed session balance.
//
// Over the mixnet chat is NON-streaming (one reply carrying the full answer). The
// client sends messages already in OpenAI shape (`{role:"user"|"assistant", content}`)
// plus its `sessionId`. Flow: enforce the session holds credit → call the provider →
// price the usage via the pricing table + margin → charge → reply with the balance.
//
// Providers: Gemini (Google's native API — the request and usage translation mirror
// src/adapters/gemini.ts + gemini-usage.ts, so the Rust server bills a Gemini exchange
// exactly like the TS server did) and OpenAI (Responses API, see openai.rs).
//
// The keyless/free test providers (Groq, Cloudflare Workers AI, Pollinations) were
// removed before mainnet on 2026-09-04: they existed to develop against without a
// billed key, none of them ever carried a paying user, and each was one more third
// party seeing prompt content for no revenue. A model this server does not route is
// now an explicit error, never a silent fallback to "some other provider".

use scrai_core::billing::{compute_billing, TokenUsage};
use scrai_core::pricing::PricingTable;
use serde_json::{json, Value};
use std::collections::HashMap;

/// Idempotent-retry reply cache bounds. A client whose reply was lost in transit
/// resends the SAME counter; we hand back the stored reply instead of re-charging.
/// Kept small + in-memory: only text-sized replies are cached (big image replies are
/// skipped), and the map is capped so it can never grow without bound.
const MAX_CACHED_REPLY: usize = 256 * 1024;
const MAX_CACHED_SESSIONS: usize = 64;

/// A request that reached the provider is never free, even if it rounds to sub-1 TOKU.
/// Default 1 TOKU ($0.00001); override per-operator with the `MIN_CHARGE_TOKU` env var
/// (e.g. a larger floor to cover per-request overhead / discourage dust spam).
const MIN_CHARGE_DEFAULT: u64 = 1;
fn min_charge() -> u64 {
    crate::cfg("MIN_CHARGE_TOKU")
        .or_else(|_| crate::cfg("MIN_CHARGE_SCRAI"))   // pre-rename .env
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(MIN_CHARGE_DEFAULT)
}

/// Hard ceiling on a client-supplied `maxTokens`. No real model answers past this,
/// and clamping at the door keeps the reserve estimate and the provider body from
/// overflowing on an absurd value (L-srv-2). Mostly self-billing protection, but the
/// arithmetic bound matters regardless.
const MAX_OUTPUT_TOKENS: u64 = 131_072;

/// Ceilings on one chat request (M-srv-2). A vision request carries base64 images
/// inline, so the byte bound is generous; the message-count bound stops a pathologem
/// array from driving unbounded clone/estimate work.
const MAX_REQUEST_BYTES: usize = 48 * 1024 * 1024;
const MAX_MESSAGES: usize = 2_000;

/// Assumed visible-answer budget when a client sends no maxTokens of its own
/// (mirrors the TS server's DEFAULT_MAX_TOKENS).
fn default_max_tokens() -> u64 {
    crate::cfg("DEFAULT_MAX_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4096)
}

/// Cap on thinking tokens (mirrors the TS server's THINKING_BUDGET). On Gemini
/// these bill AS OUTPUT but are NOT bounded by maxOutputTokens, so without a cap a
/// thinking model can generate far more billed output than the answer limit.
/// 0 disables thinking; raise it to trade cost for more reasoning depth.
fn thinking_budget() -> u64 {
    crate::cfg("THINKING_BUDGET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2048)
}

/// Hard cap on the client-chosen thinking budget. More thinking is the USER's own
/// cost (billed as their output), so this is just a sanity bound — and, crucially,
/// the SAME value drives both the request and the reserve, so settle never exceeds
/// the reservation regardless of what the client asked for.
const MAX_THINKING_BUDGET: u64 = 8192;

/// Google's published output tokens per generated image, by requested size
/// (`generationConfig.imageConfig.imageSize` on Gemini 3.x image models). The
/// reserve assumes the requested size; settle() bills the real
/// `candidatesTokensDetails` count. Nano Banana (2.5) takes no size and always
/// returns a 1K picture of 1290 tokens.
const IMAGE_SIZES: [(&str, u64); 4] = [("512", 747), ("1K", 1120), ("2K", 1680), ("4K", 2520)];
const DEFAULT_IMAGE_SIZE: &str = "1K";
const LEGACY_IMAGE_TOKENS: u64 = 1290;

/// The client's requested image size for this turn. UNSIGNED like `thinkingBudget`
/// (outside canonicalBody): it only raises or lowers the sender's own bill, and the
/// reserve is sized to it. Unknown or absent → 1K.
fn image_size_of(v: &Value) -> &'static str {
    let want = v.get("imageSize").and_then(|s| s.as_str()).unwrap_or(DEFAULT_IMAGE_SIZE);
    IMAGE_SIZES
        .iter()
        .map(|(s, _)| *s)
        .find(|s| s.eq_ignore_ascii_case(want))
        .unwrap_or(DEFAULT_IMAGE_SIZE)
}

/// Output tokens of ONE picture at `size` (Gemini 3.x image models).
pub fn image_tokens_for(size: &str) -> u64 {
    IMAGE_SIZES.iter().find(|(s, _)| *s == size).map(|(_, t)| *t).unwrap_or(1120)
}

/// Every selectable size with its token count — the catalog prices them for the picker.
pub fn image_sizes() -> &'static [(&'static str, u64)] {
    &IMAGE_SIZES
}

/// Whether `model` honours `imageConfig.imageSize` (Gemini 3.x image models; Nano
/// Banana 2.5 rejects unknown config and always draws 1K).
pub fn model_takes_image_size(model: &str) -> bool {
    model.contains("image") && !model.starts_with("gemini-2.5")
}

/// The sizes a Gemini 3.x image model actually accepts. Google answers an unsupported
/// `imageSize` with a 400 ("… not supported by this model") — observed 2026-08-27: Nano
/// Banana 2 Lite refuses 2K. Lite is a 512/1K model; the full model goes up to 4K.
pub fn supported_image_sizes(model: &str) -> &'static [&'static str] {
    if model.contains("lite") {
        &["512", "1K"]
    } else {
        &["512", "1K", "2K", "4K"]
    }
}

/// The size actually requested from the provider: the user's choice if the model
/// supports it, else the largest supported size below it (2K on Lite → 1K). Never
/// rejects — a too-big wish degrades to the best the model can do, and the reply's
/// `usage.imageSize` says what was drawn.
pub fn effective_image_size(model: &str, want: &'static str) -> &'static str {
    let supported = supported_image_sizes(model);
    if supported.contains(&want) {
        return want;
    }
    let want_idx = IMAGE_SIZES.iter().position(|(s, _)| *s == want).unwrap_or(1);
    IMAGE_SIZES[..want_idx]
        .iter()
        .rev()
        .map(|(s, _)| *s)
        .find(|s| supported.contains(s))
        .unwrap_or(DEFAULT_IMAGE_SIZE)
}

/// The thinking budget for THIS request: the client's `thinkingBudget` (clamped),
/// else the server env default. UNSIGNED (outside canonicalBody), like `live` — it
/// only ever lowers or raises the user's own bill, and the overcharge check bounds abuse.
fn effective_thinking(v: &Value) -> u64 {
    match v.get("thinkingBudget").and_then(|t| t.as_u64()) {
        Some(t) => t.min(MAX_THINKING_BUDGET),
        None => thinking_budget().min(MAX_THINKING_BUDGET),
    }
}

/// Safe upper bound on the input tokens one attachment bills as. Gemini tiles a
/// large image into ~hundreds of tokens and a PDF page costs ~258+; this
/// over-reserves rather than risk billing above the ceiling.
const ATTACHMENT_INPUT_TOKENS: u64 = 4096;

/// Worst-case price of a request, in TOKU — the amount to reserve. Byte length,
/// not chars/4: a byte-level BPE token decodes to at least one byte, so the byte
/// count is a GUARANTEED upper bound on the real input token count (an
/// adversarial multibyte prompt tokenises far above chars/4, and the user is
/// never charged above this ceiling, so it must not undercount).
/// Gemini 3 Grounding with Google Search: billed per web-search query the model
/// actually runs ($14 per 1,000 queries). The reserve assumes at most this many
/// per turn so settle never exceeds the reservation.
const GROUNDING_USD_PER_QUERY: f64 = 0.014;
const MAX_GROUNDING_QUERIES: u64 = 10;
/// Gemini's monthly free Grounding allowance (Gemini 3 series): the first 5,000
/// grounded prompts per calendar month cost us $0, so users are not charged for
/// them — only queries beyond it bill at $14/1k. Tracked per UTC month in the store.
pub const GROUNDING_FREE_PER_MONTH: u64 = 5000;

/// (provider cost in TOKU, retail TOKU charged) for `queries` executed grounding
/// searches. Zero queries → no charge, so a live-enabled prompt the model chose NOT
/// to search on costs nothing extra.
/// Per-query search price for a model's provider: Gemini grounding or OpenAI web search.
fn search_usd_per_query(model: &str) -> f64 {
    if crate::openai::is_openai_model(model) { crate::openai::search_usd_per_call() } else { GROUNDING_USD_PER_QUERY }
}

fn grounding_charge_at(queries: u64, usd_per_query: f64, margin: f64) -> (f64, u64) {
    use scrai_core::billing::{ceil_toku, clamp_margin};
    use scrai_core::coconut::TOKU_PER_USD;
    if queries == 0 {
        return (0.0, 0);
    }
    let cost = ceil_toku(queries as f64 * usd_per_query * TOKU_PER_USD as f64);
    let retail = (cost * clamp_margin(margin)).ceil() as u64;
    (cost, retail)
}

#[allow(clippy::too_many_arguments)]
fn ceiling_for(
    price: &scrai_core::billing::ModelPrice,
    margin: f64,
    messages: &Value,
    max_tokens: Option<u64>,
    live: bool,
    grounding_free: u64,
    thinking: u64,
    image_size: &str,
    model: &str,
) -> u64 {
    use scrai_core::billing::{ceil_toku, clamp_margin};
    use scrai_core::coconut::TOKU_PER_USD;
    let retail = |usd_per_million: f64| {
        ceil_toku(usd_per_million * TOKU_PER_USD as f64 * clamp_margin(margin)).ceil()
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
    // Same `thinking` value the request will actually use → reserve matches the real
    // billable output, so a lowered thinking budget really does reserve (and cost) less.
    let out_tokens = max_tokens.unwrap_or_else(default_max_tokens) + thinking;
    // Token-billed image models (Nano Banana): text + thinking reserve at the TEXT
    // rate, plus one image at the REQUESTED size at the IMAGE rate — the same split
    // settle() bills, so the reserve neither blocks nor under-covers.
    let (text_out_rate, image_tokens) = match price.output_text {
        Some(t) => (t, image_tokens_for(image_size)),
        None => (price.output, 0),
    };
    let tokens = ((in_tokens as f64 * retail(price.input)
        + out_tokens as f64 * retail(text_out_rate)
        + image_tokens as f64 * retail(price.output))
        / 1_000_000.0)
        .ceil() as u64;
    // Live grounding: reserve headroom only for the BILLABLE worst case — queries
    // beyond the month's free allowance. Under the allowance grounding is free, so a
    // low-balance user isn't falsely blocked by a reserve for cost they won't incur.
    let grounding = if live {
        grounding_charge_at(MAX_GROUNDING_QUERIES.saturating_sub(grounding_free), search_usd_per_query(model), margin).1
    } else {
        0
    };
    tokens + per_image_toku(price, margin) + grounding
}

/// Retail TOKU for ONE generated image (0 for text models).
pub fn per_image_toku(price: &scrai_core::billing::ModelPrice, margin: f64) -> u64 {
    use scrai_core::billing::{ceil_toku, clamp_margin};
    use scrai_core::coconut::TOKU_PER_USD;
    match price.per_image {
        Some(usd) if usd > 0.0 => {
            ceil_toku(usd * TOKU_PER_USD as f64 * clamp_margin(margin)).ceil() as u64
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
    let f = crate::cfg("FREE_TIER_FACTOR")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .map(|v| v.clamp(0.0, 1.0))
        .unwrap_or(0.5);
    ModelPrice {
        input: p.input * f,
        output: p.output * f,
        output_text: p.output_text.map(|t| t * f),
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
///
/// The dispatch loop (main.rs) drives reserve()/run_provider()/settle() directly so the
/// provider call runs off-thread (H2); this synchronous wrapper stays for the tests and
/// as the reference for the exact phase ordering.
#[allow(dead_code)]
pub async fn handle(
    request: &[u8],
    sessions: &mut scrai_core::session::SessionStore,
    uploads: &mut crate::uploads::UploadStore,
    pricing: &PricingTable,
    margin: f64,
    replies: &mut HashMap<String, (u64, Vec<u8>)>,
    // Grounding queries still free this UTC month (Gemini's 5,000/mo allowance minus
    // what's been used). Queries beyond it bill at $14/1k; within it they cost $0.
    grounding_free: u64,
) -> Vec<u8> {
    // Phase 1 (reserve) + phase 3 (settle) both touch the money state and are FAST;
    // only the provider call between them is slow. Splitting here lets main.rs run that
    // call off the dispatch loop (H2) while reserve/settle stay serialized on it.
    match reserve(request, sessions, uploads, pricing, margin, replies, grounding_free) {
        Reserved::Reply(bytes) => bytes,
        Reserved::Proceed(p) => {
            let result = run_provider(&p).await;
            settle(*p, result, sessions, replies).reply
        }
    }
}

/// PHASE 2 (off the loop): the slow provider call for a reserved chat. Holds no money
/// state, so main.rs can run it in a spawned task and hand the result back to settle().
pub async fn run_provider(p: &PendingChat) -> Result<(String, TokenUsage, Images), String> {
    chat(&p.v, p.messages.clone(), p.live, p.thinking, p.image_size).await
}

/// Everything settle() needs after the provider returns — carried from reserve() so the
/// slow provider call can run in a spawned task while the money state stays on the loop.
pub struct PendingChat {
    id: Value,
    v: Value,
    model: String,
    messages: Value, // uploads already resolved in
    live: bool,
    thinking: u64,
    image_size: &'static str,
    /// The client said it can fetch chunked pictures (`chunkedImages: true`); older
    /// clients leave it out and get their images inline as before.
    chunked: bool,
    price: scrai_core::billing::ModelPrice,
    margin: f64,
    pricing_version: String,
    grounding_free: u64,
    paid: Option<PaidCtx>, // None = genuinely-free tier (no session/reserve)
}
struct PaidCtx {
    session_id: String,
    counter: u64,
    ceiling: u64,
}

/// Outcome of the synchronous, loop-side reserve step.
impl PendingChat {
    /// The paying session behind this chat (None on the genuinely-free tier). Used only
    /// for the per-day distinct-users count — hashed before it touches the metrics table.
    pub fn session_id(&self) -> Option<&str> {
        self.paid.as_ref().map(|p| p.session_id.as_str())
    }

    /// Provider key for the per-provider concurrency cap (main.rs).
    pub fn provider(&self) -> &'static str {
        provider_of(&self.model)
    }
}

pub enum Reserved {
    /// Done — send this reply immediately (validation error, or an idempotent replay hit).
    Reply(Vec<u8>),
    /// Reserved (or free): run the provider HTTP, then hand the result to settle().
    Proceed(Box<PendingChat>),
}

/// PHASE 1 (loop side, fast): validate, verify the session signature, reserve the
/// worst-case ceiling, and consume staged uploads — everything that mutates the money
/// state. The slow provider call happens AFTER this returns, off the loop.
pub fn reserve(
    request: &[u8],
    sessions: &mut scrai_core::session::SessionStore,
    uploads: &mut crate::uploads::UploadStore,
    pricing: &PricingTable,
    margin: f64,
    replies: &mut HashMap<String, (u64, Vec<u8>)>,
    grounding_free: u64,
) -> Reserved {
    let v: Value = serde_json::from_slice(request).unwrap_or(Value::Null);
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    let err = |msg: &str| Reserved::Reply(encode(&json!({ "id": id.clone(), "kind": "error", "error": msg })));

    // Reject an oversized request or an absurd message count up front (M-srv-2). The
    // body is already in memory (it arrived as one mixnet reply), but this bounds the
    // downstream clone/estimate/provider work an anonymous caller can trigger.
    if request.len() > MAX_REQUEST_BYTES {
        return err("request is too large");
    }
    if v.get("messages").and_then(|m| m.as_array()).map(|a| a.len()).unwrap_or(0) > MAX_MESSAGES {
        return err("too many messages in one request");
    }

    let session_id = v.get("sessionId").and_then(|s| s.as_str()).unwrap_or("").to_string();
    let model = v.get("model").and_then(|m| m.as_str()).unwrap_or("").to_string();
    // OpenAI web search has no monthly free allowance — every call is billable. Decided
    // HERE, before the `pending` closure below captures it: until 2026-09-04 the override
    // sat after that closure, so settle() still subtracted Gemini's ~5,000 free queries
    // from OpenAI's search calls and billed none of them ($0.10 of $0.13 on one day).
    let grounding_free = if crate::openai::is_openai_model(&model) { 0 } else { grounding_free };
    let messages = v.get("messages").cloned().unwrap_or_else(|| json!([]));
    let max_tokens = v.get("maxTokens").and_then(|m| m.as_u64()).map(|m| m.min(MAX_OUTPUT_TOKENS));
    // Live web-search grounding for this turn (unsigned flag — see the client). Only
    // Gemini text models honour it; other providers ignore it downstream.
    let live = v.get("live").and_then(|b| b.as_bool()).unwrap_or(false);
    // Client-chosen reasoning depth for this turn (clamped); drives both to_gemini and
    // the reserve so a lower budget reliably means faster + cheaper.
    let thinking = effective_thinking(&v);
    // Requested picture size (image models only; ignored elsewhere). Drives both the
    // provider request and the reserve, like `thinking`.
    let image_size = effective_image_size(&model, image_size_of(&v));
    let chunked = v.get("chunkedImages").and_then(|b| b.as_bool()).unwrap_or(false);

    // A model with only the fallback price is never served — the operator can't
    // price it honestly, so charging for it would be guesswork on the user's dime.
    let price = effective_price(pricing.price(&model));
    if price.fallback {
        return err(&format!("model \"{model}\" has no price entry on this server"));
    }
    // …and a priced model still has to be one this server OFFERS. The catalog's rules
    // (PROVIDERS, the OpenAI id allowlist, "nano only" on a testnet server) shape
    // the picker; asking them again here is what makes them real. Before this check a
    // hand-crafted request reached any priced model, including ones the operator had
    // switched off — see `catalog::model_offered`.
    if !crate::catalog::model_offered(&model) {
        return err(&format!("model \"{model}\" is not offered by this server"));
    }
    let pending = |paid, messages| {
        Reserved::Proceed(Box::new(PendingChat {
            id: id.clone(),
            v: v.clone(),
            model: model.clone(),
            messages,
            live,
            thinking,
            image_size,
            chunked,
            price,
            margin,
            pricing_version: pricing.version().to_string(),
            grounding_free,
            paid,
        }))
    };

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
    // Abuse strikes: a session that collected today's quota of declines AT THIS PROVIDER
    // is refused that provider's models until tomorrow (openai.rs) — before anything is
    // reserved or sent anywhere. Other providers stay available.
    let provider = provider_of(&model);
    if crate::openai::blocked(&session_id, provider, crate::openai::day_number()) {
        return err(&format!(
            "{} models are paused for this session for the rest of the day after repeated policy declines — they work again tomorrow (UTC); other models are unaffected",
            provider_label(provider)
        ));
    }
    let ceiling = ceiling_for(&price, margin, &messages, max_tokens, live, grounding_free, thinking, image_size, &model);
    match sessions.reserve(&session_id, counter, ceiling) {
        Reserve::Ok => {}
        Reserve::Unknown => return err("unknown session — redeem coconut coins into it first"),
        Reserve::Replay { server_counter } => {
            // Idempotent retry: the client resent the request whose reply was lost. If
            // this exact counter is the one the server last processed and we still hold
            // its reply, hand it back — same answer, no second charge.
            if counter == server_counter {
                if let Some((c, bytes)) = replies.get(&session_id) {
                    if *c == counter {
                        return Reserved::Reply(bytes.clone());
                    }
                }
            }
            return err(&format!(
                "counter {counter} was already used (server is at {server_counter}) — resync and retry"
            ));
        }
        Reserve::Insufficient { balance } => {
            return err(&format!(
                // Wire string. 0.4.x clients match `contains("not enough SCRAI")` to trigger the
                // auto-redeem; keep that phrase until they are gated out (MIN_APP).
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

    pending(Some(PaidCtx { session_id, counter, ceiling }), resolved)
}

/// PHASE 3 (loop side, fast): price the real usage, settle the reservation (or refund
/// it fully on a provider failure), and cache the reply for idempotent retry. Runs back
/// on the dispatch loop, so the session counter/balance are never touched concurrently.
/// What `settle` hands back: the reply bytes for the client, plus what the provider
/// billed us for this answer — kept OUT of the reply (a release server never sends the
/// margin to a client) but needed on the loop for the daily metrics.
pub struct Settled {
    pub reply: Vec<u8>,
    /// provider cost in TOKU (None when the provider failed — nothing was billed)
    pub provider_cost: Option<f64>,
}

pub fn settle(
    p: PendingChat,
    result: Result<(String, TokenUsage, Images), String>,
    sessions: &mut scrai_core::session::SessionStore,
    replies: &mut HashMap<String, (u64, Vec<u8>)>,
) -> Settled {
    let id = p.id;
    let mut provider_cost: Option<f64> = None;
    let Some(paid) = p.paid else {
        // ---- genuinely-free tier: no session, no cache ----
        let reply = match result {
            Ok((text, usage, images)) => {
                let frame = compute_billing(&p.price, &usage, p.margin, 0, usage.estimated);
                provider_cost = Some(frame.cost_toku);
                let usage_json = json!({
                    "inputTokens": usage.input,
                    "cachedInputTokens": usage.cached_input,
                    "audioInputTokens": usage.audio_input,
                    "outputTokens": usage.output,
                    "outputImageTokens": usage.output_image,
                    "imageSize": p.image_size,
                    "billing": {
                        // Both spellings during the rename: a 0.4.6 app reads *Scrai, a
                        // newer one *Toku. Drop the Scrai pair once the fleet has moved.
                        "priceToku": frame.price_toku,
                        "costToku": dev_audit_cost(frame.cost_toku),
                        "priceScrai": frame.price_toku,
                        "costScrai": dev_audit_cost(frame.cost_toku),
                        "model": p.model,
                        "pricingVersion": p.pricing_version,
                        "estimated": frame.estimated,
                        "fallbackPrice": frame.fallback_price,
                    },
                });
                let mut r = json!({ "id": id, "text": text, "usage": usage_json, "cost": 0 });
                if let Some(imgs) = images {
                    r["images"] = imgs;
                    if p.chunked {
                        r["chunked"] = json!(true); // main.rs may stage big pictures (replies.rs)
                    }
                }
                r
            }
            Err(e) => json!({ "id": id, "kind": "error", "error": e }),
        };
        return Settled { reply: encode(&reply), provider_cost };
    };

    // ---- paid session ----
    let reply = match result {
        Ok((text, usage, images)) => {
            let mut frame = compute_billing(&p.price, &usage, p.margin, min_charge(), usage.estimated);
            // Live grounding: only queries BEYOND the monthly free allowance cost us
            // anything, so only those are billed (per query, on top of tokens). Within
            // the allowance grounding is genuinely free → nothing added.
            let billable_queries = usage.grounding_queries.saturating_sub(p.grounding_free);
            let (g_cost, g_retail) = grounding_charge_at(billable_queries, search_usd_per_query(&p.model), p.margin);
            frame.cost_toku += g_cost;
            provider_cost = Some(frame.cost_toku);
            // One line per answer, the numbers the provider's console shows — so a spend
            // mismatch is a journal grep, not a reconstruction. No content, no identifiers.
            eprintln!(
                "scrai-server: usage {} in={} cached={} out={} img={} searches={} (billable {}) cost={:.0} charged={}{}",
                p.model,
                usage.input,
                usage.cached_input,
                usage.output,
                usage.output_image,
                usage.grounding_queries,
                billable_queries,
                frame.cost_toku,
                frame.price_toku + g_retail,
                if usage.estimated { " ESTIMATED" } else { "" }
            );
            // Token cost + per-image cost (image models report zero tokens) + grounding.
            let n_images = images.as_ref().and_then(|i| i.as_array()).map(|a| a.len()).unwrap_or(0) as u64;
            let cost = frame.price_toku + n_images * per_image_toku(&p.price, p.margin) + g_retail;
            // Settle: the unused part of the reservation comes back.
            let balance = sessions.settle(&paid.session_id, paid.ceiling, cost);
            // Canonical usage the UI expects (camelCase) + a billing frame whose
            // priceScrai IS the amount charged, so footer + price + balance all agree.
            let usage_json = json!({
                "inputTokens": usage.input,
                "cachedInputTokens": usage.cached_input,
                "audioInputTokens": usage.audio_input,
                "outputTokens": usage.output,
                "outputImageTokens": usage.output_image,
                "imageSize": p.image_size,
                "groundingQueries": usage.grounding_queries,
                "billing": {
                    "priceToku": cost,
                    "costToku": dev_audit_cost(frame.cost_toku),
                    "priceScrai": cost,
                    "costScrai": dev_audit_cost(frame.cost_toku),
                    "model": p.model,
                    "pricingVersion": p.pricing_version,
                    "estimated": frame.estimated,
                    "fallbackPrice": frame.fallback_price,
                },
            });
            let mut r =
                json!({ "id": id, "text": text, "usage": usage_json, "cost": cost, "balance": balance });
            if let Some(imgs) = images {
                r["images"] = imgs;
                if p.chunked {
                    r["chunked"] = json!(true); // main.rs may stage big pictures (replies.rs)
                }
            }
            r
        }
        Err(e) => {
            // Provider failed → the user pays nothing.
            sessions.refund(&paid.session_id, paid.ceiling);
            // A policy decline (any provider, or our moderation prefilter) is a strike
            // against the session; today's quota reached = paused until tomorrow.
            if e.starts_with("Declined by") {
                crate::openai::strike(&paid.session_id, provider_of(&p.model), crate::openai::day_number());
            }
            json!({ "id": id, "kind": "error", "error": e })
        }
    };
    let out = encode(&reply);
    // Cache this reply against (session, counter) so a lost-reply retry replays it — but
    // ONLY a real answer. A provider failure was refunded, so a retry on the same counter
    // should report the counter as used (client advances + retries for a fresh attempt),
    // not replay the stale error. Bounded: skip large (image) replies and cap the map.
    let is_error = reply.get("kind").and_then(|k| k.as_str()) == Some("error");
    if !is_error && out.len() <= MAX_CACHED_REPLY {
        if replies.len() >= MAX_CACHED_SESSIONS && !replies.contains_key(&paid.session_id) {
            if let Some(k) = replies.keys().next().cloned() {
                replies.remove(&k);
            }
        }
        replies.insert(paid.session_id.clone(), (paid.counter, out.clone()));
    }
    Settled { reply: out, provider_cost }
}

fn encode(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_default()
}

/// The generated images of an image-model reply, as the client renders them:
/// `[{ "mimeType": …, "data": <base64> }]`. None for text models.
pub type Images = Option<Value>;

/// Call the provider for `v` (with upload refs already resolved into `messages`)
/// → (answer text, normalized token usage, generated images).
async fn chat(
    v: &Value,
    messages: Value,
    live: bool,
    thinking: u64,
    image_size: &str,
) -> Result<(String, TokenUsage, Images), String> {
    let model = v.get("model").and_then(|m| m.as_str()).ok_or("no model")?;
    let max_tokens = v.get("maxTokens").and_then(|m| m.as_u64()).map(|m| m.min(MAX_OUTPUT_TOKENS));

    // Load testing: a canned answer instead of a provider call (fake-payments servers only).
    if let Some((delay_ms, chars)) = mock_provider() {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        let input = serde_json::to_string(&messages).map(|s| s.len() as u64 / 4).unwrap_or(0);
        let text = mock_answer(chars);
        let usage = TokenUsage { input, output: (chars as u64 / 4).max(1), estimated: true, ..Default::default() };
        return Ok((text, usage, None));
    }

    // Route by model → provider; text catalogs are fetched live (catalog.rs),
    // image models are the static free-tier sets. Only Gemini honours `live`
    // grounding; image-gen models never search, so it's forced off for them.
    if model.starts_with("gemini") {
        let live = live && !model.contains("image");
        gemini(model, &messages, max_tokens, live, thinking, image_size).await
    } else if crate::openai::is_openai_model(model) {
        let sid = v.get("sessionId").and_then(|s| s.as_str());
        let (t, u) = crate::openai::chat(model, &messages, max_tokens.unwrap_or_else(default_max_tokens), live, thinking, sid).await?;
        Ok((t, u, None))
    } else {
        // Unroutable. `catalog::model_offered` refuses these long before here, so this
        // is the belt to that braces — never a fallback to an unnamed provider.
        Err(format!("model \"{model}\" is not served by this server"))
    }
}

/// LOAD-TEST ONLY: `MOCK_PROVIDER=<delay_ms>[:<answer_chars>]` makes every chat return
/// a canned answer after `delay_ms` instead of calling a provider — so a load test drives
/// the whole money path (reserve → settle → persist → reply over the mixnet) without a
/// single model call. Honoured ONLY together with FAKE_PAYMENTS=1: on that server no
/// real money can arrive (pay.rs refuses fake + real rails), so nobody is charged for a
/// fake answer. On any other server the variable is ignored (main.rs logs that at boot).
pub fn mock_provider() -> Option<(u64, usize)> {
    static PARSED: std::sync::OnceLock<Option<(u64, usize)>> = std::sync::OnceLock::new();
    *PARSED.get_or_init(|| {
        let raw = crate::cfg("MOCK_PROVIDER").ok()?;
        if !crate::pay::fake_payments_enabled() {
            return None;
        }
        let mut it = raw.split(':');
        let delay = it.next()?.trim().parse().ok()?;
        let chars = it.next().and_then(|c| c.trim().parse().ok()).unwrap_or(600);
        Some((delay, chars))
    })
}

fn mock_answer(chars: usize) -> String {
    const S: &str = "This is a mock answer from a load-test server; no model was called. ";
    let mut t = String::with_capacity(chars + S.len());
    while t.len() < chars {
        t.push_str(S);
    }
    t.truncate(chars);
    t
}

/// Boot-line helper: is the OpenAI moderation prefilter on?
pub fn openai_prefilter() -> bool {
    crate::openai::prefilter_enabled()
}

/// User-facing name of a provider key.
fn provider_label(provider: &str) -> &'static str {
    match provider {
        "gemini" => "Google",
        "openai" => "OpenAI",
        _ => "This provider's",
    }
}

/// Which provider serves a model — the key for per-provider concurrency caps.
pub fn provider_of(model: &str) -> &'static str {
    if model.starts_with("gemini") {
        "gemini"
    } else if crate::openai::is_openai_model(model) {
        "openai"
    } else {
        "other"
    }
}

// ---- Gemini (Google AI Studio, native API) --------------------------------

const GEMINI_BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";

/// Resolve the active Gemini key. Two named slots exist so the operator can
/// keep both keys in .env and flip by (un)commenting — but EXACTLY ONE may be
/// active: mixing a paid mainnet key and a free testnet key silently is how
/// billing accidents happen, so both-set is a hard error, checked at boot.
/// Legacy GEMINI_API_KEY still works when neither slot is set.
pub fn gemini_api_key() -> Result<(String, &'static str), String> {
    let get = |k: &str| {
        std::env::var(k)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    match (get("GEMINI_API_KEY_MAINNET"), get("GEMINI_API_KEY_TESTNET")) {
        (Some(_), Some(_)) => Err(
            "GEMINI_API_KEY_MAINNET and GEMINI_API_KEY_TESTNET are BOTH set — \
             exactly one may be active; comment the other out in .env"
                .into(),
        ),
        (Some(k), None) => Ok((k, "MAINNET")),
        (None, Some(k)) => Ok((k, "testnet")),
        (None, None) => get("GEMINI_API_KEY")
            .map(|k| (k, "legacy GEMINI_API_KEY"))
            .ok_or_else(|| "no Gemini key — set GEMINI_API_KEY_TESTNET or GEMINI_API_KEY_MAINNET".into()),
    }
}

/// Whether Gemini uses this operator's prompts for training — the client shows it as a
/// privacy badge. The PAID (mainnet) tier does NOT train on API data; the free/testnet
/// tier does (outside EU/UK/EEA). Unknown legacy key → assume it trains (conservative).
pub fn gemini_trains_on_input() -> bool {
    !matches!(gemini_api_key(), Ok((_, "MAINNET")))
}

/// Human-readable explanation for an EMPTY Gemini answer, built from its finish / block
/// reason. `None` for a plain STOP with nothing to say (nothing to explain). The text
/// starts with "Declined by Google" — the app recognises that prefix and styles it.
pub fn decline_message(finish: Option<&str>, block: Option<&str>, image_model: bool) -> Option<String> {
    let reason = block.or(finish)?;
    let why = match reason {
        "IMAGE_SAFETY" | "IMAGE_PROHIBITED_CONTENT" | "IMAGE_OTHER" =>
            "its image models don't depict recognisable real people or restricted content. Describe a fictional character or leave the name out, then try again",
        "IMAGE_RECITATION" | "RECITATION" =>
            "the result would reproduce protected material. Rephrase the request",
        "SAFETY" | "PROHIBITED_CONTENT" | "BLOCKLIST" | "SPII" | "OTHER" =>
            "the request tripped its content policy. Rephrase it and try again",
        "MAX_TOKENS" =>
            "the reply ran out of output budget while thinking. Lower the reasoning depth or ask for something shorter",
        // A clean STOP with nothing in it: on an image model that is almost always a
        // question sent to a painter — say so instead of "(no content)".
        "STOP" if image_model =>
            "this model draws pictures from a description and can't answer questions. Pick a text model (e.g. Gemini 3.5 Flash-Lite) and resend — or describe the picture you want",
        "STOP" => return None,
        _ => "no content came back",
    };
    let billed = if image_model { "Only the model's reasoning was billed — no picture." } else { "Only the tokens it used were billed." };
    let head = if reason == "STOP" { "No picture from Google (STOP)".to_string() } else { format!("Declined by Google ({reason})") };
    Some(format!("{head}: {why}. {billed}"))
}

/// Did an image model answer with a PLACEHOLDER instead of a picture? Nano Banana
/// sometimes writes a literal `{image}` (or `[image]`) into its text and returns no image
/// part at all — it narrates the picture rather than drawing it. The user must not be
/// shown that: to them it reads as a broken answer, and they were charged for tokens.
///
/// Only counts when NO image came back. An answer that has both a picture and the word
/// in its prose is fine.
pub fn image_placeholder_only(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    ["{image}", "[image]", "{{image}}", "{image_1}", "{image1}"]
        .iter()
        .any(|m| t.contains(m))
}

async fn gemini(
    model: &str,
    messages: &Value,
    max_tokens: Option<u64>,
    live: bool,
    thinking: u64,
    image_size: &str,
) -> Result<(String, TokenUsage, Images), String> {
    let (key, _) = gemini_api_key()?;
    let mut body = to_gemini(messages, max_tokens.unwrap_or_else(default_max_tokens), thinking, live);
    // Image models (Nano Banana) MUST be told to emit an image, or they inconsistently
    // reply with text only (a literal "{image}" placeholder the model writes itself) and
    // no picture. Forcing responseModalities makes the image reliably come back.
    if model.contains("image") {
        body["generationConfig"]["responseModalities"] = serde_json::json!(["TEXT", "IMAGE"]);
    }
    // Picture size (512 / 1K / 2K / 4K) — billed by Google as more output tokens, which
    // usageMetadata reports and settle() prices; the reserve already assumed this size.
    if model_takes_image_size(model) {
        body["generationConfig"]["imageConfig"] = serde_json::json!({ "imageSize": image_size });
    }

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

    let parts = j.pointer("/candidates/0/content/parts").and_then(|p| p.as_array());
    let mut text = parts
        .map(|ps| {
            ps.iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<String>()
        })
        .unwrap_or_default();
    // Image-generation models (Nano Banana / gemini-*-image) return the picture as an
    // inlineData part on the SAME generateContent endpoint — the shape the client
    // renders. Billing stays token-based: usageMetadata reports
    // the image's output tokens under modality IMAGE (billed at `out`), and the model's
    // text + thinking under TEXT / thoughtsTokenCount (billed at `out_text`) — see
    // gemini_usage(). No per-image charge is added.
    let imgs: Vec<Value> = parts
        .map(|ps| {
            ps.iter()
                .filter_map(|p| {
                    let d = p.get("inlineData")?;
                    let mime = d.get("mimeType").and_then(|m| m.as_str())?;
                    let data = d.get("data").and_then(|x| x.as_str())?;
                    Some(json!({ "mimeType": mime, "data": data }))
                })
                .collect()
        })
        .unwrap_or_default();
    // Google can answer with NO parts at all — a refused picture (recognisable real
    // people, safety), a blocked prompt, or a thinking budget that ate the whole output.
    // The reason arrives as candidates[0].finishReason / promptFeedback.blockReason;
    // surface it as the reply text so the user learns what to change instead of
    // seeing "(the model returned no content)".
    if text.trim().is_empty() && imgs.is_empty() {
        let finish = j.pointer("/candidates/0/finishReason").and_then(|f| f.as_str());
        let block = j.pointer("/promptFeedback/blockReason").and_then(|f| f.as_str());
        if let Some(msg) = decline_message(finish, block, model.contains("image")) {
            eprintln!("scrai-server: gemini {model} returned no content (finish={finish:?} block={block:?})");
            text = msg;
        }
    }
    // An image model that returned prose with a "{image}" marker and no picture has
    // narrated instead of drawn (see `image_placeholder_only`). Say so plainly rather
    // than passing the placeholder through to the chat.
    if model.contains("image") && imgs.is_empty() && image_placeholder_only(&text) {
        eprintln!("scrai-server: gemini {model} wrote an image placeholder instead of drawing one");
        text = "No picture from Google: the model described the picture instead of drawing it, \
                and wrote a placeholder where the image should have been. Send the prompt again \
                — a fresh request usually draws it. Only the tokens it used were billed."
            .to_string();
    }
    let images: Images = (!imgs.is_empty()).then(|| json!(imgs));
    let mut usage = gemini_usage(j.get("usageMetadata").unwrap_or(&Value::Null), !imgs.is_empty());
    // SAFETY (never-lose): if Google returned NO usageMetadata (edge / partial error) but we
    // DID get billable content, fall back to an estimate so the user is charged something.
    // Text → ~4 chars/token; an image → its typical 1K-size output tokens. Margin covers slack.
    let got_content = !text.is_empty() || images.is_some();
    let no_usage = usage.input == 0
        && usage.output == 0
        && usage.output_image == 0
        && usage.cached_input == 0
        && usage.audio_input == 0;
    if no_usage && got_content {
        use scrai_core::billing::estimate_tokens;
        let in_chars: u64 = messages
            .as_array()
            .map(|a| a.iter().filter_map(|m| m.get("content").and_then(|c| c.as_str())).map(|s| s.len() as u64).sum())
            .unwrap_or(0);
        let n_images = imgs.len() as u64;
        let per_image = if model_takes_image_size(model) { image_tokens_for(image_size) } else { LEGACY_IMAGE_TOKENS };
        usage = TokenUsage {
            input: estimate_tokens(in_chars),
            output: estimate_tokens(text.len() as u64),
            output_image: n_images * per_image,
            estimated: true,
            ..Default::default()
        };
        eprintln!("scrai-server: gemini returned no usageMetadata — billed on an estimate");
    }
    // Live grounding: bill for each web-search query the model actually ran (Gemini 3
    // charges per executed query). Empty-string entries are NOT billable (Google ignores
    // them), so they're excluded from the count. An empty/absent list → no grounding charge.
    if live {
        let meta = j.pointer("/candidates/0/groundingMetadata");
        usage.grounding_queries = meta
            .and_then(|m| m.get("webSearchQueries"))
            .and_then(|w| w.as_array())
            .map(|a| {
                a.iter()
                    .filter(|q| q.as_str().map(|s| !s.trim().is_empty()).unwrap_or(false))
                    .count() as u64
            })
            .unwrap_or(0);
    }
    Ok((text, usage, images))
}

/// Translate OpenAI-shape messages to Gemini's native request: `contents[].parts[]`
/// with roles "user"/"model" and a separate top-level `systemInstruction`.
fn to_gemini(messages: &Value, answer_tokens: u64, thinking: u64, live: bool) -> Value {
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
    // Live grounding: hand Gemini the Google Search tool. The MODEL decides whether to
    // actually search (billed per executed query) — so trivial prompts stay free.
    if live {
        body["tools"] = json!([{ "google_search": {} }]);
    }
    body
}

/// Translate Gemini's usageMetadata into the neutral TokenUsage. Three things the
/// naive reading gets wrong, all of them money (see src/adapters/gemini-usage.ts):
///
///   1. candidatesTokenCount does NOT include thinking. Google bills output as
///      candidates + thoughts, so that sum is the output count.
///   2. promptTokenCount INCLUDES cached tokens, which bill at ~1/10th the rate.
///      Uncached input is prompt − cached (− audio, which has its own rate).
///   3. On image models candidatesTokenCount MIXES the picture (modality IMAGE,
///      $60/1M on Nano Banana 2) with the model's text (TEXT, $3/1M); thinking is
///      text too. Billing all of it at the image rate overcharged 20× on text and
///      thinking — the 2026-08-26 reconciliation showed 3.8× Google's real bill.
///      `candidatesTokensDetails` carries the split, so IMAGE tokens go to
///      `output_image` and everything else (incl. thoughts) stays in `output`.
///
/// `got_image` says whether the reply actually carried an inlineData picture. If it
/// did but the details give NO image split (older/other response shape), the whole
/// candidates count is treated as image tokens — the pre-split behaviour, which can
/// only over-bill by the small text share, never under-bill a picture at $3/1M.
fn gemini_usage(meta: &Value, got_image: bool) -> TokenUsage {
    let n = |key: &str| meta.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
    let prompt = n("promptTokenCount") + n("toolUsePromptTokenCount");
    let cached = n("cachedContentTokenCount").min(prompt);
    let audio = audio_tokens(meta.get("promptTokensDetails")).min(prompt - cached);
    let candidates = n("candidatesTokenCount");
    let mut image = modality_tokens(meta.get("candidatesTokensDetails"), "IMAGE").min(candidates);
    if got_image && image == 0 {
        image = candidates;
    }
    TokenUsage {
        input: prompt - cached - audio,
        output: candidates - image + n("thoughtsTokenCount"),
        output_image: image,
        cached_input: cached,
        audio_input: audio,
        ..Default::default()
    }
}

fn audio_tokens(details: Option<&Value>) -> u64 {
    modality_tokens(details, "AUDIO")
}

/// Sum of `tokenCount` over the entries of a `*TokensDetails` array whose
/// `modality` matches (case-insensitive). Absent / malformed → 0.
fn modality_tokens(details: Option<&Value>, modality: &str) -> u64 {
    details
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|d| {
                    d.get("modality")
                        .and_then(|m| m.as_str())
                        .is_some_and(|m| m.eq_ignore_ascii_case(modality))
                })
                .map(|d| d.get("tokenCount").and_then(|t| t.as_u64()).unwrap_or(0))
                .sum()
        })
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    // Regression (tester, 0.4.6, 2026-09-04): the model wrote "{image}" markers into its
    // text and returned no picture. That must never reach the chat as-is.
    #[test]
    fn image_placeholders_are_recognised() {
        use super::image_placeholder_only;
        assert!(image_placeholder_only("Hier sind die Bilder:\n{image}\n{image}"));
        assert!(image_placeholder_only("[IMAGE]"));
        assert!(image_placeholder_only("see {Image_1} above"));
        // ordinary prose about images must NOT trip it
        assert!(!image_placeholder_only("Here is the image you asked for."));
        assert!(!image_placeholder_only("I drew an image of a cat."));
        assert!(!image_placeholder_only(""));
    }

    #[test]
    fn empty_gemini_answers_explain_themselves() {
        use super::decline_message;
        let m = decline_message(Some("IMAGE_SAFETY"), None, true).unwrap();
        assert!(m.starts_with("Declined by Google (IMAGE_SAFETY)"));
        assert!(m.contains("real people") && m.contains("no picture"));
        // a blocked prompt wins over the candidate's finish reason
        let m = decline_message(Some("STOP"), Some("PROHIBITED_CONTENT"), false).unwrap();
        assert!(m.contains("PROHIBITED_CONTENT") && m.contains("content policy") && !m.contains("picture"));
        assert!(decline_message(Some("MAX_TOKENS"), None, false).unwrap().contains("reasoning depth"));
        // a normal stop with nothing to say is not a decline on a TEXT model …
        assert!(decline_message(Some("STOP"), None, false).is_none());
        assert!(decline_message(None, None, true).is_none());
        // … but on an image model it means "you asked a painter a question"
        let m = decline_message(Some("STOP"), None, true).unwrap();
        assert!(m.starts_with("No picture from Google (STOP)") && m.contains("text model"));
    }

    use super::*;
    use scrai_core::billing::{ModelPrice, Tier};

    #[test]
    fn gemini_usage_bills_thoughts_as_output_and_splits_cached_input() {
        let meta = json!({
            "promptTokenCount": 1000,
            "cachedContentTokenCount": 400,
            "candidatesTokenCount": 200,
            "thoughtsTokenCount": 300,
            "totalTokenCount": 1500
        });
        let u = gemini_usage(&meta, false);
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
        let u = gemini_usage(&meta, false);
        assert_eq!(u.input, 70);
        assert_eq!(u.audio_input, 30);
    }

    #[test]
    fn gemini_usage_splits_image_tokens_from_text_and_thinking() {
        // VERBATIM usageMetadata of a real gemini-3.1-flash-image call (2026-08-27,
        // scripts/gemini-usage-probe.sh, prompt "A small red circle on white
        // background.", one 1K jpeg, no text part). Note the shape: the details
        // list ONLY the IMAGE modality (1120 = Google's published 1K count), yet
        // candidatesTokenCount is 1466 — the 346 extra are neither a text part nor
        // in the details. They stay in `output` (text rate): Google's own per-image
        // price ($0.067 = 1120 × $60/1M) proves they are NOT billed as image tokens.
        let meta = json!({
            "promptTokenCount": 9,
            "candidatesTokenCount": 1466,
            "totalTokenCount": 1812,
            "promptTokensDetails": [{ "modality": "TEXT", "tokenCount": 9 }],
            "candidatesTokensDetails": [{ "modality": "IMAGE", "tokenCount": 1120 }],
            "thoughtsTokenCount": 337,
            "serviceTier": "standard"
        });
        let u = gemini_usage(&meta, true);
        assert_eq!(u.input, 9);
        assert_eq!(u.output_image, 1120); // billed at the image rate
        assert_eq!(u.output, 346 + 337); // non-image candidates + thoughts — text rate
        // Priced like pricing.json's Nano Banana 2: $0.5 in, $60 image, $3 text.
        let nb2 = ModelPrice {
            input: 0.5,
            output: 60.0,
            output_text: Some(3.0),
            cached: None,
            audio: None,
            fallback: false,
            tier: Tier::Paid,
            per_image: None,
        };
        let usd = scrai_core::billing::cost_usd(&u, &nb2);
        // 9×0.5 + 1120×60 + 683×3 = 4.5 + 67_200 + 2_049 = 69_253.5 → $0.0692535
        assert!((usd - 0.0692535).abs() < 1e-12, "{usd}");
        // The pre-fix accounting billed (1466 + 337) × $60/1M = $0.10818 for the same
        // reply — 1.56× Google's price; on Nano Banana 2 Lite (same shape, 1449 + 539
        // candidates/thoughts at $30 image / $1.50 text) it was 3.4×.
        assert!(usd < 0.0693);
    }

    #[test]
    fn gemini_usage_without_modality_details_keeps_everything_as_text_output() {
        // A text model never sends candidatesTokensDetails with IMAGE → output_image 0.
        let meta = json!({ "promptTokenCount": 10, "candidatesTokenCount": 20 });
        let u = gemini_usage(&meta, false);
        assert_eq!(u.output, 20);
        assert_eq!(u.output_image, 0);
    }

    #[test]
    fn a_returned_image_without_modality_split_is_billed_entirely_as_image_tokens() {
        // Never-undercharge: Google sent a picture but no candidatesTokensDetails →
        // the whole candidates count is image tokens; thinking stays text.
        let meta = json!({ "promptTokenCount": 10, "candidatesTokenCount": 1180, "thoughtsTokenCount": 500 });
        let u = gemini_usage(&meta, true);
        assert_eq!(u.output_image, 1180);
        assert_eq!(u.output, 500);
    }

    #[test]
    fn image_model_reserve_uses_the_text_rate_for_thinking_plus_one_worst_case_image() {
        let text = ModelPrice {
            input: 0.5,
            output: 60.0,
            output_text: None,
            cached: None,
            audio: None,
            fallback: false,
            tier: Tier::Paid,
            per_image: None,
        };
        let nb2 = ModelPrice { output_text: Some(3.0), ..text };
        let msgs = json!([{ "role": "user", "content": "a cat" }]);
        let old = ceiling_for(&text, 1.0, &msgs, Some(1024), false, 5000, 2048, "4K", "gemini-x");
        let new = ceiling_for(&nb2, 1.0, &msgs, Some(1024), false, 5000, 2048, "4K", "gemini-x");
        // old: 3072 output tokens × $60/1M = $0.184 (≈ 18_432 TOKU) — all at the image rate
        // new: 3072 × $3/1M + 2520 × $60/1M = $0.0092 + $0.1512 ≈ 16_044 TOKU
        assert!(new < old, "split reserve {new} should be below the all-image-rate reserve {old}");
        assert!(new >= 2520 * 60 / 10, "reserve must still cover a 4K image: {new}");
        // The reserve follows the requested size: 1K (1120 tokens) reserves less than 4K.
        let one_k = ceiling_for(&nb2, 1.0, &msgs, Some(1024), false, 5000, 2048, "1K", "gemini-x");
        assert!(one_k < new, "1K reserve {one_k} must be below 4K reserve {new}");
        assert!(one_k >= 1120 * 60 / 10);
    }

    #[test]
    fn image_size_is_validated_and_defaults_to_1k() {
        assert_eq!(image_size_of(&json!({})), "1K");
        assert_eq!(image_size_of(&json!({ "imageSize": "4K" })), "4K");
        assert_eq!(image_size_of(&json!({ "imageSize": "2k" })), "2K");
        assert_eq!(image_size_of(&json!({ "imageSize": "512" })), "512");
        assert_eq!(image_size_of(&json!({ "imageSize": "8K" })), "1K");
        assert_eq!(image_size_of(&json!({ "imageSize": 7 })), "1K");
        assert_eq!(image_tokens_for("512"), 747);
        assert_eq!(image_tokens_for("4K"), 2520);
        assert!(model_takes_image_size("gemini-3.1-flash-image"));
        assert!(model_takes_image_size("gemini-3.1-flash-lite-image"));
        assert!(!model_takes_image_size("gemini-2.5-flash-image"));
        assert!(!model_takes_image_size("gemini-3.5-flash"));
    }

    #[test]
    fn unsupported_sizes_degrade_to_the_models_largest_supported_size() {
        // Nano Banana 2 Lite: Google refuses 2K/4K → 1K; 512 and 1K pass through.
        assert_eq!(effective_image_size("gemini-3.1-flash-lite-image", "2K"), "1K");
        assert_eq!(effective_image_size("gemini-3.1-flash-lite-image", "4K"), "1K");
        assert_eq!(effective_image_size("gemini-3.1-flash-lite-image", "512"), "512");
        assert_eq!(effective_image_size("gemini-3.1-flash-lite-image", "1K"), "1K");
        // Nano Banana 2 takes everything.
        assert_eq!(effective_image_size("gemini-3.1-flash-image", "4K"), "4K");
        assert_eq!(effective_image_size("gemini-3.1-flash-image", "2K"), "2K");
    }

    #[test]
    fn gemini_usage_never_underflows_on_inconsistent_counts() {
        // A cached count larger than prompt must clamp, not wrap around u64.
        let meta = json!({ "promptTokenCount": 10, "cachedContentTokenCount": 50 });
        let u = gemini_usage(&meta, false);
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
        let body = to_gemini(&messages, 4096, 2048, false);

        assert_eq!(body.pointer("/systemInstruction/parts/0/text").unwrap(), "be brief");
        assert!(body.get("tools").is_none()); // no grounding tool unless live
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
        let body = to_gemini(&json!([{ "role": "user", "content": "hi" }]), 100, 0, false);
        assert!(body.get("systemInstruction").is_none());
        assert_eq!(body.pointer("/contents/0/parts/0/text").unwrap(), "hi");
    }

    #[test]
    fn to_gemini_attaches_google_search_tool_when_live() {
        let body = to_gemini(&json!([{ "role": "user", "content": "weather tomorrow?" }]), 100, 0, true);
        assert_eq!(body.pointer("/tools/0/google_search").unwrap(), &json!({}));
    }

    #[test]
    fn grounding_charge_bills_per_query_and_zero_is_free() {
        assert_eq!(grounding_charge_at(0, GROUNDING_USD_PER_QUERY, 1.1), (0.0, 0));
        // 2 queries × $0.014 = $0.028 → provider TOKU > 0, retail = ceil(cost×1.1)
        let (cost, retail) = grounding_charge_at(2, GROUNDING_USD_PER_QUERY, 1.1);
        assert!(cost > 0.0);
        assert!(retail as f64 >= cost); // margin never lowers the charge
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
        let mut replies: std::collections::HashMap<String, (u64, Vec<u8>)> = std::collections::HashMap::new();
        let pricing = PricingTable::parse(
            r#"{"version":"t","default":{"in":1.0,"out":4.0,"fallback":true},
                "models":{"gemini-m":{"in":1.0,"out":4.0},"gemini-m2":{"in":1.0,"out":4.0}}}"#,
        )
        .unwrap();
        sessions.credit(&sid, 100_000);

        // Unsigned → refused before anything happens. (The ids are `gemini-*` so the
        // request is one this server actually routes — `catalog::model_offered` refuses
        // an unroutable id before the paywall is reached at all.)
        let bare = json!({"kind":"chat","id":"x","model":"gemini-m","messages":[]}).to_string();
        let r: Value = serde_json::from_slice(
            &handle(bare.as_bytes(), &mut sessions, &mut uploads, &pricing, 1.4, &mut replies, GROUNDING_FREE_PER_MONTH).await,
        )
        .unwrap();
        assert!(r["error"].as_str().unwrap().contains("signed session"));

        // Tampered model (signature covers the body) → refused, balance untouched.
        // (Tampers to another PRICED model — an unpriced one is rejected by the
        // price check before the signature is even looked at.)
        let mut env: Value = serde_json::from_slice(&signed_chat(&sk, &pem, &sid, 1, "gemini-m")).unwrap();
        env["model"] = json!("gemini-m2");
        let r: Value = serde_json::from_slice(
            &handle(env.to_string().as_bytes(), &mut sessions, &mut uploads, &pricing, 1.4, &mut replies, GROUNDING_FREE_PER_MONTH).await,
        )
        .unwrap();
        assert!(r["error"].as_str().unwrap().contains("signature"));
        assert_eq!(sessions.balance(&sid), 100_000);

        // Valid signature: reserve happens, provider fails (no Gemini key in the test
        // env) → FULL refund, but the counter is consumed.
        let r: Value = serde_json::from_slice(
            &handle(&signed_chat(&sk, &pem, &sid, 1, "gemini-m"), &mut sessions, &mut uploads, &pricing, 1.4, &mut replies, GROUNDING_FREE_PER_MONTH).await,
        )
        .unwrap();
        assert!(r.get("error").is_some());
        assert_eq!(sessions.balance(&sid), 100_000); // refunded
        assert_eq!(sessions.status(&sid).1, 1); // counter advanced

        // Replaying the same counter is now refused.
        let r: Value = serde_json::from_slice(
            &handle(&signed_chat(&sk, &pem, &sid, 1, "gemini-m"), &mut sessions, &mut uploads, &pricing, 1.4, &mut replies, GROUNDING_FREE_PER_MONTH).await,
        )
        .unwrap();
        assert!(r["error"].as_str().unwrap().contains("already used"));
    }

    // The unit rename ships server-first, so one reply has to satisfy both: a 0.4.6 app
    // reading *Scrai and a newer one reading *Toku. Same number under both names.
    #[tokio::test]
    async fn a_reply_carries_the_billing_numbers_under_both_names() {
        let (sk, pem, sid) = session_keypair();
        let mut sessions = scrai_core::session::SessionStore::default();
        let mut uploads = crate::uploads::UploadStore::default();
        let mut replies: std::collections::HashMap<String, (u64, Vec<u8>)> = std::collections::HashMap::new();
        let pricing = PricingTable::parse(
            r#"{"version":"t","default":{"in":1.0,"out":4.0,"fallback":true},"models":{"gemini-b":{"in":1.0,"out":4.0}}}"#,
        )
        .unwrap();
        sessions.credit(&sid, 1_000_000);
        let Reserved::Proceed(p) = reserve(&signed_chat(&sk, &pem, &sid, 1, "gemini-b"), &mut sessions, &mut uploads, &pricing, 1.4, &mut replies, GROUNDING_FREE_PER_MONTH)
        else {
            panic!("should reserve");
        };
        let usage = TokenUsage { input: 5_000, output: 5_000, ..Default::default() };
        let r: Value = serde_json::from_slice(
            &settle(*p, Ok(("hi".to_string(), usage, None)), &mut sessions, &mut replies).reply,
        )
        .unwrap();
        let b = &r["usage"]["billing"];
        assert!(b["priceToku"].as_u64().unwrap() > 0, "priced");
        assert_eq!(b["priceToku"], b["priceScrai"], "old and new name must agree");
        assert_eq!(b["costToku"], b["costScrai"]);
    }

    // Regression (2026-09-04): OpenAI web-search calls were billed as if Gemini's monthly
    // free allowance applied — the OpenAI override was decided after the closure that
    // carries grounding_free into settle() had already captured the Gemini value.
    #[tokio::test]
    async fn openai_search_calls_are_billed_despite_gemini_free_allowance() {
        let (sk, pem, sid) = session_keypair();
        let mut sessions = scrai_core::session::SessionStore::default();
        let mut uploads = crate::uploads::UploadStore::default();
        let mut replies: std::collections::HashMap<String, (u64, Vec<u8>)> = std::collections::HashMap::new();
        let pricing = PricingTable::parse(
            r#"{"version":"t","default":{"in":1.0,"out":4.0,"fallback":true},"models":{"gpt-5.6-luna":{"in":0.2,"out":1.20}}}"#,
        )
        .unwrap();
        sessions.credit(&sid, 1_000_000);
        // 4,990 Gemini queries still free this month — must not leak into OpenAI billing
        let Reserved::Proceed(p) = reserve(&signed_chat(&sk, &pem, &sid, 1, "gpt-5.6-luna"), &mut sessions, &mut uploads, &pricing, 1.4, &mut replies, 4_990)
        else {
            panic!("should reserve");
        };
        assert_eq!(p.grounding_free, 0, "OpenAI has no free search allowance");
        let usage = TokenUsage { input: 32_000, output: 4_000, grounding_queries: 3, ..Default::default() };
        let settled = settle(*p, Ok(("hi".to_string(), usage, None)), &mut sessions, &mut replies);
        let cost = settled.provider_cost.unwrap();
        // tokens: 32k × $0.20/M + 4k × $1.25/M = $0.0114 = 1,140 TOKU; searches: 3 × $0.01 = 3,000 TOKU
        assert!(cost >= 4_100.0 && cost < 4_200.0, "provider cost must include the three search calls, got {cost}");
        let r: Value = serde_json::from_slice(&settled.reply).unwrap();
        assert!(r["cost"].as_u64().unwrap() > 3_000, "the user is charged for the searches too");
    }

    // ---- H2 concurrency: reserve() and settle() are split so the provider call can run
    // off the dispatch loop; these prove the money math stays exact when two chats overlap.

    #[tokio::test]
    async fn concurrent_reserves_hold_both_then_settle_without_double_spend() {
        let (sk, pem, sid) = session_keypair();
        let mut sessions = scrai_core::session::SessionStore::default();
        let mut uploads = crate::uploads::UploadStore::default();
        let mut replies: std::collections::HashMap<String, (u64, Vec<u8>)> = std::collections::HashMap::new();
        let pricing = PricingTable::parse(
            r#"{"version":"t","default":{"in":1.0,"out":4.0,"fallback":true},"models":{"gemini-m":{"in":1.0,"out":4.0}}}"#,
        )
        .unwrap();
        sessions.credit(&sid, 1_000_000);
        let start = sessions.balance(&sid);

        // Two chats reserved back-to-back (counter 1 then 2) — the H2 window where BOTH
        // worst-case reservations are held at once, before either provider call returns.
        let Reserved::Proceed(a) = reserve(&signed_chat(&sk, &pem, &sid, 1, "gemini-m"), &mut sessions, &mut uploads, &pricing, 1.4, &mut replies, GROUNDING_FREE_PER_MONTH)
        else {
            panic!("A should reserve");
        };
        let Reserved::Proceed(b) = reserve(&signed_chat(&sk, &pem, &sid, 2, "gemini-m"), &mut sessions, &mut uploads, &pricing, 1.4, &mut replies, GROUNDING_FREE_PER_MONTH)
        else {
            panic!("B should reserve");
        };
        let held = sessions.balance(&sid);
        assert!(held < start, "both reservations are held at the same time");
        assert_eq!(sessions.status(&sid).1, 2, "counter advanced by both reserves");

        // Settle both as if the provider returned a tiny answer (order A then B).
        let usage = TokenUsage { input: 5, output: 5, ..Default::default() };
        let sa = settle(*a, Ok(("hi".to_string(), usage, None)), &mut sessions, &mut replies);
        let ra: Value = serde_json::from_slice(&sa.reply).unwrap();
        // the provider cost travels beside the reply, never inside it (release servers)
        assert!(sa.provider_cost.unwrap() > 0.0);
        assert!(ra["usage"]["billing"]["costScrai"].is_null() || crate::cfg("DEV_AUDIT").as_deref() == Ok("1"));
        let rb: Value = serde_json::from_slice(&settle(*b, Ok(("hi".to_string(), usage, None)), &mut sessions, &mut replies).reply).unwrap();

        let cost_a = ra["cost"].as_u64().unwrap();
        let cost_b = rb["cost"].as_u64().unwrap();
        assert!(cost_a > 0 && cost_b > 0, "each real turn charges something");
        let end = sessions.balance(&sid);
        // No double-spend, no leaked reservation: final == start − (costA + costB).
        assert_eq!(end, start - cost_a - cost_b);
        assert_eq!(rb["balance"].as_u64().unwrap(), end, "reported balance matches the store");
    }

    #[tokio::test]
    async fn replay_after_settle_returns_cached_reply_without_recharging() {
        let (sk, pem, sid) = session_keypair();
        let mut sessions = scrai_core::session::SessionStore::default();
        let mut uploads = crate::uploads::UploadStore::default();
        let mut replies: std::collections::HashMap<String, (u64, Vec<u8>)> = std::collections::HashMap::new();
        let pricing = PricingTable::parse(
            r#"{"version":"t","default":{"in":1.0,"out":4.0,"fallback":true},"models":{"gemini-m":{"in":1.0,"out":4.0}}}"#,
        )
        .unwrap();
        sessions.credit(&sid, 1_000_000);

        // A first, successful turn (counter 1): reserve → settle.
        let Reserved::Proceed(p) = reserve(&signed_chat(&sk, &pem, &sid, 1, "gemini-m"), &mut sessions, &mut uploads, &pricing, 1.4, &mut replies, GROUNDING_FREE_PER_MONTH)
        else {
            panic!("should reserve");
        };
        let usage = TokenUsage { input: 5, output: 5, ..Default::default() };
        let first = settle(*p, Ok(("hi".to_string(), usage, None)), &mut sessions, &mut replies).reply;
        let bal_after = sessions.balance(&sid);

        // A lost-reply retry resends the SAME counter → the cached reply, and NO second charge.
        match reserve(&signed_chat(&sk, &pem, &sid, 1, "gemini-m"), &mut sessions, &mut uploads, &pricing, 1.4, &mut replies, GROUNDING_FREE_PER_MONTH) {
            Reserved::Reply(bytes) => assert_eq!(bytes, first, "replay returns the exact cached reply"),
            Reserved::Proceed(_) => panic!("replay must NOT re-run the provider"),
        }
        assert_eq!(sessions.balance(&sid), bal_after, "replay does not charge again");
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
        // Per-image retail: 0.0005 USD × 100_000 TOKU/USD × margin 1.4 = 70 TOKU.
        assert_eq!(per_image_toku(&ft, 1.4), 70);
        assert_eq!(per_image_toku(&pd, 1.4), 0);
    }

    #[tokio::test]
    async fn unpriced_models_are_rejected_and_every_priced_model_needs_a_session() {
        let mut sessions = scrai_core::session::SessionStore::default();
        let mut uploads = crate::uploads::UploadStore::default();
        let mut replies: std::collections::HashMap<String, (u64, Vec<u8>)> = std::collections::HashMap::new();
        let pricing = PricingTable::parse(
            r#"{"version":"t","default":{"in":1.0,"out":4.0,"fallback":true},
                "models":{"gemini-3.5-flash":{"in":0.3,"out":2.5},"free-thing":{"in":0.0,"out":0.0,"tier":"free"}}}"#,
        )
        .unwrap();

        // Fallback-priced model → refused outright, even before the auth check.
        let req = json!({"kind":"chat","id":"x","model":"mystery","messages":[]}).to_string();
        let r: Value = serde_json::from_slice(
            &handle(req.as_bytes(), &mut sessions, &mut uploads, &pricing, 1.4, &mut replies, GROUNDING_FREE_PER_MONTH).await,
        )
        .unwrap();
        assert!(r["error"].as_str().unwrap().contains("no price entry"));

        // A priced model with no signature: refused. There is no longer ANY request
        // shape that reaches a provider unauthenticated — the keyless test providers
        // and their `Tier::Free` shortcut were removed before mainnet (2026-09-04).
        let req = json!({"kind":"chat","id":"x","model":"gemini-3.5-flash","messages":[]}).to_string();
        let r: Value = serde_json::from_slice(
            &handle(req.as_bytes(), &mut sessions, &mut uploads, &pricing, 1.4, &mut replies, GROUNDING_FREE_PER_MONTH).await,
        )
        .unwrap();
        assert!(r["error"].as_str().unwrap().contains("funded, signed session"));

        // …and a leftover `"tier":"free"` in a pricing file no longer opens that door:
        // the string now parses as Paid, so this model needs a session like any other.
        let req = json!({"kind":"chat","id":"x","model":"free-thing","messages":[]}).to_string();
        let r: Value = serde_json::from_slice(
            &handle(req.as_bytes(), &mut sessions, &mut uploads, &pricing, 1.4, &mut replies, GROUNDING_FREE_PER_MONTH).await,
        )
        .unwrap();
        let e = r["error"].as_str().unwrap();
        assert!(e.contains("funded, signed session") || e.contains("not offered"), "got: {e}");
    }
}

/// What the provider billed us for one answer. That number is the margin in plain sight,
/// so it leaves the server ONLY when `DEV_AUDIT=1` (a developer's own server);
/// release servers send null and the app's cost-audit overlay has nothing to show.
fn dev_audit_cost(cost_toku: f64) -> serde_json::Value {
    if crate::cfg("DEV_AUDIT").as_deref() == Ok("1") {
        serde_json::json!(cost_toku)
    } else {
        serde_json::Value::Null
    }
}
