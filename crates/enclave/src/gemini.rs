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
/// The prefix of a call id made up here because Google gave none; never sent back.
const SYNTHETIC_ID: &str = "gcall_";

/// The `read_memory` calls among an answer's parts, in the neutral shape.
pub fn tool_calls(parts: &[Value]) -> Vec<Value> {
    parts
        .iter()
        .enumerate()
        .filter_map(|(i, p)| {
            let fc = p.get("functionCall")?;
            if fc.get("name").and_then(|n| n.as_str()) != Some(crate::memory::TOOL) {
                return None;
            }
            let file = fc.pointer("/args/file_id").and_then(|f| f.as_str()).unwrap_or("");
            let id = fc.get("id").and_then(|x| x.as_str()).map(str::to_string).unwrap_or_else(|| format!("{SYNTHETIC_ID}{i}"));
            Some(crate::memory::call(&id, file, p.get("thoughtSignature").and_then(|s| s.as_str())))
        })
        .collect()
}

/// The function standing in for Google's search when the palace is offered too.
pub const SEARCH_TOOL: &str = "web_search";

pub fn search_declaration() -> Value {
    json!({
        "name": SEARCH_TOOL,
        "description": "Search the web for current or factual information you do not have: news, prices, dates, facts about the world. Not for anything about the user: that is in the Memory Palace.",
        "parameters": { "type": "object", "properties": { "query": { "type": "string", "description": "What to search for, as a short web search query." } }, "required": ["query"] },
    })
}

/// The `web_search` calls among an answer's parts: (query, id if Google gave one).
pub fn search_calls(parts: &[Value]) -> Vec<(String, Option<String>)> {
    parts
        .iter()
        .filter_map(|p| {
            let fc = p.get("functionCall")?;
            if fc.get("name").and_then(|n| n.as_str()) != Some(SEARCH_TOOL) {
                return None;
            }
            let q: String = fc.pointer("/args/query").and_then(|q| q.as_str()).unwrap_or("").chars().take(300).collect();
            Some((q, fc.get("id").and_then(|i| i.as_str()).map(str::to_string)))
        })
        .collect()
}

/// What one search found, as the function's answer: Google's grounded summary, then its
/// sources, cut to size.
pub fn search_result(j: &Value, text: &str) -> String {
    let mut out: String = text.trim().chars().take(crate::policy::GEMINI_SEARCH_RESULT_CHARS).collect();
    let sources: Vec<String> = j
        .pointer("/candidates/0/groundingMetadata/groundingChunks")
        .and_then(|c| c.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|c| {
                    let w = c.get("web")?;
                    Some(format!("- {}: {}", w.get("title").and_then(|t| t.as_str()).unwrap_or(""), w.get("uri").and_then(|u| u.as_str())?))
                })
                .take(8)
                .collect()
        })
        .unwrap_or_default();
    if !sources.is_empty() {
        out.push_str("\n\nSources:\n");
        out.push_str(&sources.join("\n"));
    }
    if out.trim().is_empty() {
        out = "The search found nothing.".into();
    }
    out
}

pub fn to_gemini(messages: &Value, answer_tokens: u64, thinking: u64, live: bool) -> Value {
    let msgs = messages.as_array().map(Vec::as_slice).unwrap_or(&[]);
    let role_of = |m: &Value| m.get("role").and_then(|r| r.as_str()).unwrap_or("user").to_string();
    let system = msgs.iter().filter(|m| role_of(m) == "system").filter_map(|m| m.get("content").and_then(|c| c.as_str())).collect::<Vec<_>>().join("\n");
    // A `read_memory` call is the model's turn (`functionCall`, with Google's thought
    // signature handed back as it came); its results, however many in a row, are one user
    // turn of `functionResponse` parts, in the order the calls were made (`memory`).
    let mut contents: Vec<Value> = Vec::new();
    for m in msgs.iter().filter(|m| role_of(m) != "system") {
        let role = role_of(m);
        if role == "tool" {
            let name = m.get("name").and_then(|n| n.as_str()).unwrap_or(crate::memory::TOOL);
            let mut resp = json!({ "functionResponse": { "name": name, "response": { "content": m.get("content").and_then(|c| c.as_str()).unwrap_or("") } } });
            if let Some(id) = m.get("toolCallId").and_then(|i| i.as_str()).filter(|i| !i.starts_with(SYNTHETIC_ID)) {
                resp["functionResponse"]["id"] = json!(id);
            }
            let merge = contents.last().is_some_and(|c: &Value| c["role"] == "user" && c["parts"].as_array().is_some_and(|p| p.iter().all(|x| x.get("functionResponse").is_some())));
            if merge {
                if let Some(parts) = contents.last_mut().and_then(|c| c["parts"].as_array_mut()) {
                    parts.push(resp);
                }
            } else {
                contents.push(json!({ "role": "user", "parts": [resp] }));
            }
            continue;
        }
        let mut parts: Vec<Value> = Vec::new();
        for att in m.get("attachments").and_then(|a| a.as_array()).map(Vec::as_slice).unwrap_or(&[]) {
            if let (Some(mime), Some(data)) = (att.get("mimeType").and_then(|x| x.as_str()), att.get("data").and_then(|x| x.as_str())) {
                parts.push(json!({ "inlineData": { "mimeType": mime, "data": data } }));
            }
        }
        if let Some(text) = m.get("content").and_then(|c| c.as_str()).filter(|t| !t.is_empty()) {
            parts.push(json!({ "text": text }));
        }
        for c in m.get("toolCalls").and_then(|t| t.as_array()).map(Vec::as_slice).unwrap_or(&[]) {
            let mut fc = json!({ "name": crate::memory::TOOL, "args": { "file_id": crate::memory::file_id_of(c) } });
            if let Some(id) = c.get("id").and_then(|i| i.as_str()).filter(|i| !i.starts_with(SYNTHETIC_ID)) {
                fc["id"] = json!(id);
            }
            let mut part = json!({ "functionCall": fc });
            if let Some(sig) = c.get("sig").and_then(|s| s.as_str()) {
                part["thoughtSignature"] = json!(sig);
            }
            parts.push(part);
        }
        if parts.is_empty() {
            parts.push(json!({ "text": "" }));
        }
        contents.push(json!({ "role": if role == "assistant" { "model" } else { "user" }, "parts": parts }));
    }
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

/// Two usages as one: an answer that took two requests is billed for both.
fn add(a: TokenUsage, b: TokenUsage) -> TokenUsage {
    TokenUsage {
        input: a.input + b.input,
        output: a.output + b.output,
        cached_input: a.cached_input + b.cached_input,
        audio_input: a.audio_input + b.audio_input,
        output_image: a.output_image + b.output_image,
        grounding_queries: a.grounding_queries + b.grounding_queries,
        estimated: a.estimated || b.estimated,
    }
}

impl Gemini {
    /// One request to Google, parsed: the text, the pictures, and the raw answer (for its
    /// finish reason, its usage and its grounding).
    async fn once(&self, model: &str, body: &Value) -> Result<(String, Vec<Value>, Value), String> {
        let (text, imgs, j, _) = self.once_with_calls(model, body).await?;
        Ok((text, imgs, j))
    }

    /// One `web_search`, carried out here: the same model, asked with Google's own search
    /// and nothing else of the conversation — only the query leaves.
    async fn web_search(&self, model: &str, query: &str) -> (String, TokenUsage, u64) {
        let safety: Vec<Value> = crate::policy::GEMINI_SAFETY.iter().map(|(c, t)| json!({ "category": c, "threshold": t })).collect();
        let body = json!({
            "contents": [{ "role": "user", "parts": [{ "text": format!("Search the web for: {query}\n\nReport what you find that answers it, with dates where they matter. Plain text, no preamble.") }] }],
            "tools": [{ "google_search": {} }],
            "safetySettings": safety,
            "generationConfig": { "maxOutputTokens": crate::policy::GEMINI_SEARCH_ANSWER_TOKENS },
        });
        match self.once(model, &body).await {
            Ok((text, _, j)) => {
                let queries = j
                    .pointer("/candidates/0/groundingMetadata/webSearchQueries")
                    .and_then(|w| w.as_array())
                    .map(|a| a.iter().filter(|q| q.as_str().is_some_and(|s| !s.trim().is_empty())).count() as u64)
                    .unwrap_or(0);
                (search_result(&j, &text), usage(j.get("usageMetadata").unwrap_or(&Value::Null), false), queries)
            }
            Err(e) => (format!("The search failed: {e}"), TokenUsage::default(), 0),
        }
    }

    async fn once_with_calls(&self, model: &str, body: &Value) -> Result<(String, Vec<Value>, Value, Vec<Value>), String> {
        let res = crate::http::client()
            .post(format!("{BASE}/{model}:generateContent"))
            .header("x-goog-api-key", &self.key)
            .timeout(std::time::Duration::from_millis(TIMEOUT_MS))
            .json(body)
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
        let text: String = parts.iter().filter_map(|p| p.get("text").and_then(|t| t.as_str())).collect();
        let imgs: Vec<Value> = parts
            .iter()
            .filter_map(|p| {
                let d = p.get("inlineData")?;
                Some(json!({ "mimeType": d.get("mimeType")?.as_str()?, "data": d.get("data")?.as_str()? }))
            })
            .collect();
        let calls = tool_calls(parts);
        Ok((text, imgs, j, calls))
    }
}

/// An image model that answered in words with a plain STOP and no block: it described the
/// picture instead of drawing it. Not a decline — a decline names its reason.
fn narrated(model: &str, text: &str, j: &Value) -> bool {
    is_image_model(model)
        && !text.trim().is_empty()
        && j.pointer("/candidates/0/finishReason").and_then(|f| f.as_str()) == Some("STOP")
        && j.pointer("/promptFeedback/blockReason").is_none()
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
            // Search and the palace together: Google's search cannot share a request with a
            // function, so search becomes a function carried out here (`web_search`).
            let relay_search = req.live && call.memory.is_some() && !is_image_model(model);
            let mut body = to_gemini(&req.messages, req.answer_tokens(), req.thinking_tokens(), req.live && !relay_search);
            if is_image_model(model) {
                // Without this the image models sometimes answer in text only.
                body["generationConfig"]["responseModalities"] = json!(["TEXT", "IMAGE"]);
            }
            if model_takes_image_size(model) {
                body["generationConfig"]["imageConfig"] = json!({ "imageSize": call.image_size });
            }
            if let Some(memory) = &call.memory {
                let mut tools = body.get("tools").and_then(|t| t.as_array()).cloned().unwrap_or_default();
                let mut decl = crate::memory::gemini_tool();
                let mut memory = memory.clone();
                if relay_search {
                    if let Some(list) = decl["functionDeclarations"].as_array_mut() {
                        list.push(search_declaration());
                    }
                    memory.push_str("\nFor current or factual information about the world, call web_search; for anything about the user, read the Memory Palace first.\n");
                }
                tools.push(decl);
                body["tools"] = json!(tools);
                let system = body.pointer("/systemInstruction/parts/0/text").and_then(|t| t.as_str()).map(|t| format!("{t}\n\n{memory}")).unwrap_or_else(|| memory.clone());
                body["systemInstruction"] = json!({ "parts": [{ "text": system }] });
                if crate::memory::exhausted(&req.messages) {
                    body["toolConfig"] = json!({ "functionCallingConfig": { "mode": "NONE" } });
                }
            }
            let (mut text, mut imgs, mut j, mut calls) = self.once_with_calls(model, &body).await?;
            // The searches the model asks for are done here, round by round, until it answers
            // or asks for a memory file (which the app answers: those go back as before).
            let mut before = TokenUsage::default();
            let mut searched = 0u64;
            if relay_search {
                for round in 0..crate::policy::GEMINI_SEARCH_ROUNDS {
                    let parts = j.pointer("/candidates/0/content/parts").and_then(|p| p.as_array()).cloned().unwrap_or_default();
                    let wanted = search_calls(&parts);
                    if wanted.is_empty() || !calls.is_empty() {
                        break;
                    }
                    before = add(before, usage(j.get("usageMetadata").unwrap_or(&Value::Null), false));
                    let mut answers: Vec<Value> = Vec::new();
                    for (query, id) in wanted.iter().take(3) {
                        let (found, su, q) = self.web_search(model, query).await;
                        before = add(before, su);
                        searched += q.max(1);
                        let mut r = json!({ "functionResponse": { "name": SEARCH_TOOL, "response": { "content": found } } });
                        if let Some(id) = id {
                            r["functionResponse"]["id"] = json!(id);
                        }
                        answers.push(r);
                    }
                    // Calls beyond three are answered too: Google wants an answer to each.
                    for (_, id) in wanted.iter().skip(3) {
                        let mut r = json!({ "functionResponse": { "name": SEARCH_TOOL, "response": { "content": "Not searched: three searches at a time." } } });
                        if let Some(id) = id {
                            r["functionResponse"]["id"] = json!(id);
                        }
                        answers.push(r);
                    }
                    if let Some(contents) = body["contents"].as_array_mut() {
                        contents.push(json!({ "role": "model", "parts": parts }));
                        contents.push(json!({ "role": "user", "parts": answers }));
                    }
                    if round + 1 == crate::policy::GEMINI_SEARCH_ROUNDS {
                        body["toolConfig"] = json!({ "functionCallingConfig": { "mode": "NONE" } });
                    }
                    (text, imgs, j, calls) = self.once_with_calls(model, &body).await?;
                }
            }
            let mut u = add(before, usage(j.get("usageMetadata").unwrap_or(&Value::Null), !imgs.is_empty()));
            // "I have regenerated the image…" — and no image in the answer (2026-09-29, an
            // edit on gemini-3.1-flash-lite-image). Even with IMAGE among the response
            // modalities the model sometimes narrates instead. One more attempt, because a
            // fresh request usually draws it; both are billed, because both were made.
            if imgs.is_empty() && narrated(model, &text, &j) {
                let (t2, i2, j2) = self.once(model, &body).await?;
                u = add(u, usage(j2.get("usageMetadata").unwrap_or(&Value::Null), !i2.is_empty()));
                j = j2;
                if i2.is_empty() {
                    text = format!(
                        "{}\n\nNo picture came back, twice: the model described the change instead of drawing it. \
                         Send the request again — only the tokens it used were billed.",
                        t2.trim()
                    );
                } else {
                    text = t2;
                    imgs = i2;
                }
            }
            if text.trim().is_empty() && imgs.is_empty() && calls.is_empty() {
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
            if relay_search {
                u.grounding_queries = searched;
            } else if req.live {
                u.grounding_queries = j
                    .pointer("/candidates/0/groundingMetadata/webSearchQueries")
                    .and_then(|w| w.as_array())
                    .map(|a| a.iter().filter(|q| q.as_str().is_some_and(|s| !s.trim().is_empty())).count() as u64)
                    .unwrap_or(0);
            }
            Ok(Completion { text, usage: u, images: (!imgs.is_empty()).then(|| json!(imgs)), tool_calls: calls })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Words with a plain STOP from an image model are a narration, to be asked again;
    /// a decline, an empty answer, or a text model are not.
    #[test]
    fn a_narrated_picture_is_told_from_a_decline() {
        let stop = json!({ "candidates": [{ "finishReason": "STOP" }] });
        assert!(narrated("gemini-3.1-flash-lite-image", "I have regenerated the image", &stop));
        assert!(!narrated("gemini-3.1-flash-lite-image", "  ", &stop));
        assert!(!narrated("gemini-3.5-flash-lite", "I have regenerated the image", &stop));
        let blocked = json!({ "candidates": [{ "finishReason": "STOP" }], "promptFeedback": { "blockReason": "SAFETY" } });
        assert!(!narrated("gemini-3.1-flash-lite-image", "no", &blocked));
        let safety = json!({ "candidates": [{ "finishReason": "IMAGE_SAFETY" }] });
        assert!(!narrated("gemini-3.1-flash-lite-image", "no", &safety));
        let two = add(TokenUsage { input: 10, output: 5, ..Default::default() }, TokenUsage { input: 1, output_image: 7, estimated: true, ..Default::default() });
        assert_eq!((two.input, two.output, two.output_image, two.estimated), (11, 5, 7, true));
    }

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

    /// Search as a function: the calls are read off the answer, and what Google found
    /// comes back with its sources, cut to size.
    #[test]
    fn web_search_calls_and_their_answers() {
        let parts = vec![
            json!({ "functionCall": { "name": "web_search", "args": { "query": "Volvo EX60 range" }, "id": "c1" }, "thoughtSignature": "sig" }),
            json!({ "functionCall": { "name": "read_memory", "args": { "file_id": "f_car" } } }),
            json!({ "text": "Let me check." }),
        ];
        assert_eq!(search_calls(&parts), vec![("Volvo EX60 range".to_string(), Some("c1".to_string()))]);
        assert_eq!(tool_calls(&parts).len(), 1, "read_memory is still the app's");
        let j = json!({ "candidates": [{ "groundingMetadata": { "groundingChunks": [{ "web": { "title": "volvocars.com", "uri": "https://example.test/a" } }] } }] });
        let r = search_result(&j, "Up to 810 km (WLTP).");
        assert!(r.starts_with("Up to 810 km") && r.contains("Sources:\n- volvocars.com: https://example.test/a"), "{r}");
        assert_eq!(search_result(&json!({}), "  "), "The search found nothing.");
        let long = "x".repeat(crate::policy::GEMINI_SEARCH_RESULT_CHARS + 500);
        assert_eq!(search_result(&json!({}), &long).len(), crate::policy::GEMINI_SEARCH_RESULT_CHARS);
    }
}
