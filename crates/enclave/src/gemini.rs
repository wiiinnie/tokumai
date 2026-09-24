//! Google Gemini, through the native API: text, pictures (the "image" models) and
//! grounding with Google Search. Carried over from the first server, which verified it
//! against Google's bills to the cent (2026-08-27) — the usage reading in `usage` is where
//! the money is, so it is unchanged.

use crate::provider::{BoxFuture, Call, Completion, Provider};
use serde_json::{json, Value};
use tokumai_core::billing::{estimate_tokens, image_tokens_for, TokenUsage, DEFAULT_IMAGE_SIZE, IMAGE_SIZES};

const BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";
/// Pictures at 2K and above can take minutes.
const TIMEOUT_MS: u64 = 300_000;
/// Nano Banana (2.5) takes no size and always draws a 1K picture of this many tokens.
const LEGACY_IMAGE_TOKENS: u64 = 1290;

pub fn is_gemini_model(model: &str) -> bool {
    model.starts_with("gemini")
}

pub fn is_image_model(model: &str) -> bool {
    model.contains("image")
}

/// Gemini 3.x image models take `imageConfig.imageSize`; 2.5 rejects it.
pub fn model_takes_image_size(model: &str) -> bool {
    is_image_model(model) && !model.starts_with("gemini-2.5")
}

/// Lite draws 1K only: 512 and 2K are both a 400 from Google ("Image size 512 is not
/// supported for this model", seen live 2026-09-22 — the first server believed 512 worked).
/// The full model goes from 512 to 4K.
pub fn supported_image_sizes(model: &str) -> &'static [&'static str] {
    if model.contains("lite") {
        &["1K"]
    } else {
        &["512", "1K", "2K", "4K"]
    }
}

/// The size actually asked for: the wish if the model supports it, else the largest
/// supported size below it. Never refuses.
pub fn effective_image_size(model: &str, want: Option<&str>) -> &'static str {
    let want = IMAGE_SIZES.iter().map(|(s, _)| *s).find(|s| want.is_some_and(|w| s.eq_ignore_ascii_case(w))).unwrap_or(DEFAULT_IMAGE_SIZE);
    let supported = supported_image_sizes(model);
    if supported.contains(&want) {
        return want;
    }
    let idx = IMAGE_SIZES.iter().position(|(s, _)| *s == want).unwrap_or(1);
    IMAGE_SIZES[..idx].iter().rev().map(|(s, _)| *s).find(|s| supported.contains(s)).unwrap_or(DEFAULT_IMAGE_SIZE)
}

/// Output tokens one picture of this model and size costs, for the reservation.
pub fn image_tokens(model: &str, size: &str) -> u64 {
    if !is_image_model(model) {
        0
    } else if model_takes_image_size(model) {
        image_tokens_for(size)
    } else {
        LEGACY_IMAGE_TOKENS
    }
}

/// Why an answer came back empty, from its finish or block reason; `None` for a plain STOP.
pub fn decline_message(finish: Option<&str>, block: Option<&str>, image_model: bool) -> Option<String> {
    let reason = block.or(finish)?;
    let why = match reason {
        "IMAGE_SAFETY" | "IMAGE_PROHIBITED_CONTENT" | "IMAGE_OTHER" => {
            "its image models don't depict recognisable real people or restricted content. Describe a fictional character or leave the name out, then try again"
        }
        "IMAGE_RECITATION" | "RECITATION" => "the result would reproduce protected material. Rephrase the request",
        "SAFETY" | "PROHIBITED_CONTENT" | "BLOCKLIST" | "SPII" | "OTHER" => "the request tripped its content policy. Rephrase it and try again",
        "MAX_TOKENS" => "the reply ran out of output budget while thinking. Lower the reasoning depth or ask for something shorter",
        "STOP" if image_model => {
            "this model draws pictures from a description and can't answer questions. Pick a text model and resend — or describe the picture you want"
        }
        "STOP" => return None,
        _ => "no content came back",
    };
    let billed = if image_model { "Only the model's reasoning was billed — no picture." } else { "Only the tokens it used were billed." };
    let head = if reason == "STOP" { "No picture from Google (STOP)".to_string() } else { format!("Declined by Google ({reason})") };
    Some(format!("{head}: {why}. {billed}"))
}

/// An image model that wrote a placeholder instead of drawing.
pub fn image_placeholder_only(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    ["{image}", "[image]", "{{image}}", "{image_1}", "{image1}"].iter().any(|m| t.contains(m))
}

/// Our messages → Gemini's request: `contents[].parts[]`, roles user/model, and the system
/// turns as `systemInstruction`. Thinking counts against `maxOutputTokens` on Gemini 3, so
/// the answer gets its budget on top of the thinking budget.
pub fn to_gemini(messages: &Value, answer_tokens: u64, thinking: u64, live: bool) -> Value {
    let msgs = messages.as_array().map(Vec::as_slice).unwrap_or(&[]);
    let role_of = |m: &Value| m.get("role").and_then(|r| r.as_str()).unwrap_or("user").to_string();
    let system = msgs.iter().filter(|m| role_of(m) == "system").filter_map(|m| m.get("content").and_then(|c| c.as_str())).collect::<Vec<_>>().join("\n");
    let contents: Vec<Value> = msgs
        .iter()
        .filter(|m| role_of(m) != "system")
        .map(|m| {
            let mut parts: Vec<Value> = Vec::new();
            for att in m.get("attachments").and_then(|a| a.as_array()).map(Vec::as_slice).unwrap_or(&[]) {
                if let (Some(mime), Some(data)) = (att.get("mimeType").and_then(|x| x.as_str()), att.get("data").and_then(|x| x.as_str())) {
                    parts.push(json!({ "inlineData": { "mimeType": mime, "data": data } }));
                }
            }
            if let Some(text) = m.get("content").and_then(|c| c.as_str()).filter(|t| !t.is_empty()) {
                parts.push(json!({ "text": text }));
            }
            if parts.is_empty() {
                parts.push(json!({ "text": "" }));
            }
            json!({ "role": if role_of(m) == "assistant" { "model" } else { "user" }, "parts": parts })
        })
        .collect();
    // Google's filters are off unless we ask for them (see `policy::GEMINI_SAFETY`), so this
    // is not a tightening of a default — it is the only filter in front of a Gemini question.
    let safety: Vec<Value> = crate::policy::GEMINI_SAFETY.iter().map(|(c, t)| json!({ "category": c, "threshold": t })).collect();
    let mut body = json!({ "contents": contents, "safetySettings": safety,
                           "generationConfig": { "maxOutputTokens": answer_tokens + thinking } });
    // A budget of 0 is refused by Gemini 3 models ("invalid argument", seen 2026-09-22):
    // leave the thinking settings out and let the output cap bound it instead.
    if thinking > 0 {
        body["generationConfig"]["thinkingConfig"] = json!({ "thinkingBudget": thinking });
    }
    if !system.is_empty() {
        body["systemInstruction"] = json!({ "parts": [{ "text": system }] });
    }
    if live {
        body["tools"] = json!([{ "google_search": {} }]);
    }
    body
}

fn modality_tokens(details: Option<&Value>, modality: &str) -> u64 {
    details
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|d| d.get("modality").and_then(|m| m.as_str()).is_some_and(|m| m.eq_ignore_ascii_case(modality)))
                .map(|d| d.get("tokenCount").and_then(|t| t.as_u64()).unwrap_or(0))
                .sum()
        })
        .unwrap_or(0)
}

/// Gemini's `usageMetadata` → ours. Three traps, all of them money: candidates exclude
/// thinking (output = candidates + thoughts); prompt includes cached tokens (billed at a
/// tenth); on image models candidates mix the picture (IMAGE) with text (billed far lower),
/// split by `candidatesTokensDetails`.
pub fn usage(meta: &Value, got_image: bool) -> TokenUsage {
    let n = |key: &str| meta.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
    let prompt = n("promptTokenCount") + n("toolUsePromptTokenCount");
    let cached = n("cachedContentTokenCount").min(prompt);
    let audio = modality_tokens(meta.get("promptTokensDetails"), "AUDIO").min(prompt - cached);
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

pub struct Gemini {
    pub key: String,
}

impl Provider for Gemini {
    fn name(&self) -> &'static str {
        "gemini"
    }
    fn serves(&self, model: &str) -> bool {
        is_gemini_model(model)
    }
    fn complete<'a>(&'a self, call: &'a Call<'a>) -> BoxFuture<'a, Result<Completion, String>> {
        Box::pin(async move {
            let req = call.req;
            let model = req.model.as_str();
            let mut body = to_gemini(&req.messages, req.answer_tokens(), req.thinking_tokens(), req.live);
            if is_image_model(model) {
                // Without this the image models sometimes answer in text only.
                body["generationConfig"]["responseModalities"] = json!(["TEXT", "IMAGE"]);
            }
            if model_takes_image_size(model) {
                body["generationConfig"]["imageConfig"] = json!({ "imageSize": call.image_size });
            }
            let res = crate::http::client()
                .post(format!("{BASE}/{model}:generateContent"))
                .header("x-goog-api-key", &self.key)
                .timeout(std::time::Duration::from_millis(TIMEOUT_MS))
                .json(&body)
                .send()
                .await
                .map_err(|_| "Google could not be reached".to_string())?;
            let status = res.status();
            let j: Value = res.json().await.map_err(|_| format!("Google sent an unreadable answer ({status})"))?;
            if !status.is_success() {
                eprintln!("tokumai-enclave: gemini {status}");
                let msg = j.pointer("/error/message").and_then(|m| m.as_str()).unwrap_or("unknown error");
                return Err(format!("Google {status}: {}", msg.chars().take(200).collect::<String>()));
            }
            let parts = j.pointer("/candidates/0/content/parts").and_then(|p| p.as_array()).map(Vec::as_slice).unwrap_or(&[]);
            let mut text: String = parts.iter().filter_map(|p| p.get("text").and_then(|t| t.as_str())).collect();
            let imgs: Vec<Value> = parts
                .iter()
                .filter_map(|p| {
                    let d = p.get("inlineData")?;
                    Some(json!({ "mimeType": d.get("mimeType")?.as_str()?, "data": d.get("data")?.as_str()? }))
                })
                .collect();
            if text.trim().is_empty() && imgs.is_empty() {
                let finish = j.pointer("/candidates/0/finishReason").and_then(|f| f.as_str());
                let block = j.pointer("/promptFeedback/blockReason").and_then(|f| f.as_str());
                if let Some(msg) = decline_message(finish, block, is_image_model(model)) {
                    text = msg;
                }
            }
            if is_image_model(model) && imgs.is_empty() && image_placeholder_only(&text) {
                text = "No picture from Google: the model described the picture instead of drawing it. Send the prompt again — \
                        a fresh request usually draws it. Only the tokens it used were billed."
                    .into();
            }
            let mut u = usage(j.get("usageMetadata").unwrap_or(&Value::Null), !imgs.is_empty());
            // No usage reported but content came back: bill an estimate, never nothing.
            if u.input + u.output + u.output_image + u.cached_input + u.audio_input == 0 && (!text.is_empty() || !imgs.is_empty()) {
                let in_chars: u64 = req.messages.as_array().map(|a| a.iter().filter_map(|m| m.get("content").and_then(|c| c.as_str())).map(|s| s.len() as u64).sum()).unwrap_or(0);
                u = TokenUsage {
                    input: estimate_tokens(in_chars),
                    output: estimate_tokens(text.len() as u64),
                    output_image: imgs.len() as u64 * image_tokens(model, call.image_size),
                    estimated: true,
                    ..Default::default()
                };
            }
            if req.live {
                u.grounding_queries = j
                    .pointer("/candidates/0/groundingMetadata/webSearchQueries")
                    .and_then(|w| w.as_array())
                    .map(|a| a.iter().filter(|q| q.as_str().is_some_and(|s| !s.trim().is_empty())).count() as u64)
                    .unwrap_or(0);
            }
            Ok(Completion { text, usage: u, images: (!imgs.is_empty()).then(|| json!(imgs)) })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_gemini_request_carries_googles_own_filters() {
        // Without this Google runs with its adjustable filters OFF, so its absence would
        // not be a laxer setting — it would be no setting at all.
        let b = to_gemini(&json!([{ "role": "user", "content": "hi" }]), 100, 0, false);
        let set = b["safetySettings"].as_array().expect("safetySettings must be sent");
        assert_eq!(set.len(), crate::policy::GEMINI_SAFETY.len());
        assert!(set.iter().all(|s| s["threshold"].as_str().is_some_and(|t| t.starts_with("BLOCK_"))), "never OFF or BLOCK_NONE");
        let explicit = set.iter().find(|s| s["category"] == "HARM_CATEGORY_SEXUALLY_EXPLICIT").unwrap();
        assert_eq!(explicit["threshold"], "BLOCK_MEDIUM_AND_ABOVE", "the App Store's rule, not ours");
    }

    #[test]
    fn usage_splits_picture_text_thinking_and_cache() {
        let meta = json!({
            "promptTokenCount": 1000, "cachedContentTokenCount": 800, "candidatesTokenCount": 1400,
            "thoughtsTokenCount": 300,
            "candidatesTokensDetails": [ { "modality": "IMAGE", "tokenCount": 1120 }, { "modality": "TEXT", "tokenCount": 280 } ]
        });
        let u = usage(&meta, true);
        assert_eq!((u.input, u.cached_input, u.output_image, u.output), (200, 800, 1120, 580));
    }

    #[test]
    fn a_picture_without_a_split_is_billed_as_picture() {
        let u = usage(&json!({ "promptTokenCount": 10, "candidatesTokenCount": 1290 }), true);
        assert_eq!((u.output_image, u.output), (1290, 0));
    }

    #[test]
    fn image_sizes_degrade_to_what_the_model_can_draw() {
        assert_eq!(effective_image_size("gemini-3.1-flash-lite-image", Some("2K")), "1K");
        assert_eq!(effective_image_size("gemini-3.1-flash-lite-image", Some("512")), "1K", "Lite has nothing smaller");
        assert_eq!(effective_image_size("gemini-3.1-flash-image", Some("4k")), "4K");
        assert_eq!(effective_image_size("gemini-3.1-flash-image", None), "1K");
        assert_eq!(image_tokens("gemini-2.5-flash-image", "4K"), 1290);
        assert_eq!(image_tokens("gemini-3.5-flash-lite", "1K"), 0);
    }

    #[test]
    fn empty_answers_explain_themselves() {
        assert!(decline_message(Some("SAFETY"), None, false).unwrap().starts_with("Declined by Google (SAFETY)"));
        assert!(decline_message(Some("STOP"), None, false).is_none());
        assert!(decline_message(Some("STOP"), None, true).unwrap().contains("draws pictures"));
        assert!(image_placeholder_only("Here you go: {image}"));
    }

    #[test]
    fn the_request_carries_system_turns_apart_and_thinking_on_top() {
        let b = to_gemini(&json!([{ "role": "system", "content": "be brief" }, { "role": "user", "content": "hi" }]), 100, 50, true);
        assert_eq!(b["systemInstruction"]["parts"][0]["text"], "be brief");
        assert_eq!(b["contents"].as_array().unwrap().len(), 1);
        assert_eq!(b["generationConfig"]["maxOutputTokens"], 150);
        assert!(b["tools"][0].get("google_search").is_some());
    }

    #[test]
    fn no_thinking_budget_means_no_thinking_settings_at_all() {
        let b = to_gemini(&json!([{ "role": "user", "content": "hi" }]), 100, 0, false);
        assert!(b["generationConfig"].get("thinkingConfig").is_none());
        assert_eq!(b["generationConfig"]["maxOutputTokens"], 100);
    }
}
