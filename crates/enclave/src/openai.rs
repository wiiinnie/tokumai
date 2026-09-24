//! OpenAI, through the Responses API, and OpenAI's free moderation check.
//!
//! Carried over from the first server (verified against developers.openai.com on
//! 2026-09-03). What shapes it: reasoning tokens bill as output and can take a minute;
//! caching is automatic (`cached_tokens` bill at the cached rate); web search is a tool
//! billed per call; `store: false` keeps nothing in OpenAI's response store.
//!
//! Logging rule, stricter than before: the enclave's log leaves the enclave, so it carries
//! status codes only — never a provider's message, which can quote what it was sent.

use crate::provider::{BoxFuture, Call, Completion, Provider};
use serde_json::{json, Value};
use tokumai_core::billing::TokenUsage;

const RESPONSES_URL: &str = "https://api.openai.com/v1/responses";
const MODERATIONS_URL: &str = "https://api.openai.com/v1/moderations";
/// Reasoning answers can take a minute or two.
const TIMEOUT_MS: u64 = 180_000;

/// Is this model id served by OpenAI? `gpt-oss-*` are open-weight models OpenAI does not
/// serve through this API.
pub fn is_openai_model(model: &str) -> bool {
    if model.starts_with("gpt-oss") || model.contains('/') {
        return false;
    }
    if model.starts_with("gpt-") {
        return true;
    }
    ["o1", "o3", "o4"].iter().any(|p| model.starts_with(p) && model[p.len()..].chars().next().is_none_or(|c| c == '-'))
}

/// The app's thinking budget → `reasoning.effort` (the app's three stops land on
/// low / medium / high).
pub fn effort_for(thinking: u64) -> &'static str {
    if thinking <= 1024 {
        "low"
    } else if thinking <= 8192 {
        "medium"
    } else {
        "high"
    }
}

/// Our messages → Responses `input`. Images become `input_image`, PDFs `input_file`.
pub fn to_input(messages: &Value) -> Value {
    let items: Vec<Value> = messages
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .map(|m| {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let text = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
            match (role, m.get("attachments").and_then(|a| a.as_array())) {
                ("assistant", _) => json!({ "role": "assistant", "content": text }),
                (_, Some(atts)) if !atts.is_empty() => {
                    let mut parts = vec![json!({ "type": "input_text", "text": text })];
                    for (i, att) in atts.iter().enumerate() {
                        let mime = att.get("mimeType").and_then(|x| x.as_str()).unwrap_or("");
                        let data = att.get("data").and_then(|x| x.as_str()).unwrap_or("");
                        if mime.starts_with("image/") {
                            parts.push(json!({ "type": "input_image", "image_url": format!("data:{mime};base64,{data}") }));
                        } else if mime == "application/pdf" {
                            parts.push(json!({ "type": "input_file", "filename": format!("attachment-{}.pdf", i + 1), "file_data": format!("data:{mime};base64,{data}") }));
                        }
                    }
                    json!({ "role": role, "content": parts })
                }
                _ => json!({ "role": role, "content": text }),
            }
        })
        .collect();
    Value::Array(items)
}

pub fn output_text(j: &Value) -> String {
    j.get("output")
        .and_then(|o| o.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter(|item| item.get("type").and_then(|t| t.as_str()) == Some("message"))
        .flat_map(|item| item.get("content").and_then(|c| c.as_array()).cloned().unwrap_or_default())
        .filter(|part| part.get("type").and_then(|t| t.as_str()) == Some("output_text"))
        .filter_map(|part| part.get("text").and_then(|t| t.as_str()).map(str::to_string))
        .collect()
}

pub fn search_calls(j: &Value) -> u64 {
    j.get("output")
        .and_then(|o| o.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter(|item| item.get("type").and_then(|t| t.as_str()) == Some("web_search_call"))
        .filter(|item| item.get("status").and_then(|s| s.as_str()).is_none_or(|s| s == "completed"))
        .count() as u64
}

/// OpenAI's usage → ours. `cached_tokens` are counted inside `input_tokens`, so they are
/// taken out and billed at the cached rate.
pub fn parse_usage(j: &Value) -> TokenUsage {
    let u = j.get("usage").cloned().unwrap_or(Value::Null);
    let input_total = u.get("input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
    let cached = u.pointer("/input_tokens_details/cached_tokens").and_then(|t| t.as_u64()).unwrap_or(0).min(input_total);
    TokenUsage {
        input: input_total - cached,
        cached_input: cached,
        output: u.get("output_tokens").and_then(|t| t.as_u64()).unwrap_or(0),
        grounding_queries: search_calls(j),
        estimated: u.is_null(),
        ..Default::default()
    }
}

pub struct OpenAi {
    pub key: String,
}

impl Provider for OpenAi {
    fn name(&self) -> &'static str {
        "openai"
    }
    fn serves(&self, model: &str) -> bool {
        is_openai_model(model)
    }
    fn complete<'a>(&'a self, call: &'a Call<'a>) -> BoxFuture<'a, Result<Completion, String>> {
        Box::pin(async move {
            let req = call.req;
            let thinking = req.thinking_tokens();
            let mut body = json!({
                "model": req.model,
                "input": to_input(&req.messages),
                "max_output_tokens": req.answer_tokens() + thinking,
                "reasoning": { "effort": effort_for(thinking) },
                "store": false,
            });
            if req.live {
                body["tools"] = json!([{ "type": "web_search" }]);
            }
            if let Some(id) = &call.safety_id {
                body["safety_identifier"] = json!(id);
            }
            let res = crate::http::client()
                .post(RESPONSES_URL)
                .bearer_auth(&self.key)
                .timeout(std::time::Duration::from_millis(TIMEOUT_MS))
                .json(&body)
                .send()
                .await
                .map_err(|_| "OpenAI could not be reached".to_string())?;
            let status = res.status();
            let retry_after = res.headers().get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
            let j: Value = res.json().await.map_err(|_| format!("OpenAI sent an unreadable answer ({status})"))?;
            if !status.is_success() {
                let code = j.pointer("/error/code").and_then(|c| c.as_str()).unwrap_or("");
                let msg = j.pointer("/error/message").and_then(|m| m.as_str()).unwrap_or("unknown error");
                eprintln!("tokumai-enclave: openai {status} code={code:?}");
                return Err(match status.as_u16() {
                    429 | 502 | 503 | 504 => match retry_after {
                        Some(n) => format!("OpenAI is limiting requests right now — please try again in about {n} seconds"),
                        None => "OpenAI is busy right now — please try again in a minute".into(),
                    },
                    400 | 403 if code.contains("policy") || msg.to_lowercase().contains("policy") || msg.to_lowercase().contains("safety") => {
                        format!("Declined by OpenAI (policy): {}", msg.chars().take(160).collect::<String>())
                    }
                    400 if code == "context_length_exceeded" || msg.contains("context length") => {
                        "the conversation is too long for this model — start a new chat or prune the history".into()
                    }
                    401 => "OpenAI rejected the operator's API key".into(),
                    _ => format!("OpenAI {status}: {}", msg.chars().take(200).collect::<String>()),
                });
            }
            let text = output_text(&j);
            let usage = parse_usage(&j);
            if text.is_empty() {
                let reason = j.pointer("/incomplete_details/reason").and_then(|r| r.as_str()).unwrap_or("");
                return Err(match reason {
                    "content_filter" => "Declined by OpenAI (content filter)".into(),
                    "max_output_tokens" => "the reply ran out of output budget while thinking. Lower the reasoning depth or ask for something shorter".into(),
                    _ => "OpenAI returned an empty answer".into(),
                });
            }
            Ok(Completion { text, usage, images: None })
        })
    }
}

/// OpenAI's moderation endpoint, run on the latest user turn before any model is asked.
pub struct Moderation {
    pub key: String,
}

fn last_user_turn(messages: &Value) -> Option<Value> {
    let m = messages.as_array()?.iter().rev().find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))?;
    let mut parts = vec![json!({ "type": "text", "text": m.get("content").and_then(|c| c.as_str()).unwrap_or("") })];
    for att in m.get("attachments").and_then(|a| a.as_array()).map(Vec::as_slice).unwrap_or(&[]) {
        let mime = att.get("mimeType").and_then(|x| x.as_str()).unwrap_or("");
        let data = att.get("data").and_then(|x| x.as_str()).unwrap_or("");
        if mime.starts_with("image/") {
            parts.push(json!({ "type": "image_url", "image_url": { "url": format!("data:{mime};base64,{data}") } }));
        }
    }
    Some(Value::Array(parts))
}

/// OpenAI's category slugs (`self-harm/intent`, `illicit/violent`) as something a person can
/// read. A slug is a taxonomy for machines; somebody whose question was just refused is owed
/// a word, not a path. An unknown slug is passed through rather than dropped — a category we
/// have not seen before should still reach the person it is about.
pub fn plain_categories(cats: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for slug in cats.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let plain = match slug {
            s if s.starts_with("sexual/minors") => "sexual content involving minors",
            s if s.starts_with("sexual") => "sexual content",
            s if s.starts_with("self-harm") => "self-harm",
            s if s.starts_with("harassment") => "harassment",
            s if s.starts_with("hate") => "hate speech",
            s if s.starts_with("illicit/violent") => "instructions for violence",
            s if s.starts_with("illicit") => "instructions for something illegal",
            s if s.starts_with("violence") => "violence",
            other => other,
        };
        if !out.contains(&plain) {
            out.push(plain);
        }
    }
    match out.len() {
        0 => "its content policy".into(),
        1 => out[0].to_string(),
        n => format!("{} and {}", out[..n - 1].join(", "), out[n - 1]),
    }
}

impl Moderation {
    /// `Ok(Some(categories))` when flagged. An outage fails OPEN (logged): the model's own
    /// policy check still stands behind it.
    pub async fn flagged(&self, messages: &Value) -> Result<Option<String>, String> {
        let Some(input) = last_user_turn(messages) else { return Ok(None) };
        let res = crate::http::client()
            .post(MODERATIONS_URL)
            .bearer_auth(&self.key)
            .timeout(std::time::Duration::from_secs(20))
            .json(&json!({ "model": "omni-moderation-latest", "input": input }))
            .send()
            .await;
        let j: Value = match res {
            Ok(r) if r.status().is_success() => r.json().await.unwrap_or(Value::Null),
            Ok(r) => {
                eprintln!("tokumai-enclave: moderation {} — skipped for this request", r.status());
                return Ok(None);
            }
            Err(_) => {
                eprintln!("tokumai-enclave: moderation unreachable — skipped for this request");
                return Ok(None);
            }
        };
        let Some(result) = j.pointer("/results/0") else { return Ok(None) };
        if result.get("flagged").and_then(|f| f.as_bool()) != Some(true) {
            return Ok(None);
        }
        let cats: Vec<String> = result
            .get("categories")
            .and_then(|c| c.as_object())
            .map(|o| o.iter().filter(|(_, v)| v.as_bool() == Some(true)).map(|(k, _)| k.clone()).collect())
            .unwrap_or_default();
        Ok(Some(if cats.is_empty() { "policy".into() } else { cats.join(", ") }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_question_is_told_in_words_not_in_slugs() {
        assert_eq!(plain_categories("self-harm, self-harm/intent"), "self-harm", "one word, not the whole tree");
        assert_eq!(plain_categories("hate/threatening, violence"), "hate speech and violence");
        assert_eq!(plain_categories("illicit/violent"), "instructions for violence");
        assert_eq!(plain_categories("sexual/minors"), "sexual content involving minors");
        assert_eq!(plain_categories(""), "its content policy", "never an empty parenthesis");
        assert_eq!(plain_categories("something_new"), "something_new", "an unknown category still reaches the person");
    }

    #[test]
    fn model_routing_recognises_openai_ids_only() {
        assert!(is_openai_model("gpt-5.4-mini"));
        assert!(is_openai_model("o3"));
        assert!(!is_openai_model("gpt-oss-120b"));
        assert!(!is_openai_model("openai/gpt-oss-120b"));
        assert!(!is_openai_model("gemini-3.5-flash-lite"));
        assert!(!is_openai_model("o4x"));
    }

    #[test]
    fn effort_follows_the_thinking_slider() {
        assert_eq!((effort_for(0), effort_for(4096), effort_for(16_000)), ("low", "medium", "high"));
    }

    #[test]
    fn usage_and_text_parse_from_a_responses_reply() {
        let j = json!({
            "output": [
                { "type": "web_search_call", "status": "completed" },
                { "type": "message", "content": [ { "type": "output_text", "text": "Hello " }, { "type": "output_text", "text": "world" } ] }
            ],
            "usage": { "input_tokens": 120, "input_tokens_details": { "cached_tokens": 100 }, "output_tokens": 1186 }
        });
        assert_eq!(output_text(&j), "Hello world");
        let u = parse_usage(&j);
        assert_eq!((u.input, u.cached_input, u.output, u.grounding_queries), (20, 100, 1186, 1));
    }

    #[test]
    fn input_conversion_keeps_roles_and_maps_attachments() {
        let msgs = json!([
            { "role": "system", "content": "be brief" },
            { "role": "user", "content": "look", "attachments": [
                { "mimeType": "image/png", "data": "AAAA" }, { "mimeType": "application/pdf", "data": "BBBB" } ] },
            { "role": "assistant", "content": "ok" }
        ]);
        let inp = to_input(&msgs);
        assert_eq!(inp[0]["content"], "be brief");
        assert_eq!(inp[1]["content"][1]["image_url"], "data:image/png;base64,AAAA");
        assert_eq!(inp[1]["content"][2]["type"], "input_file");
        assert_eq!(inp[2]["content"], "ok");
    }
}
