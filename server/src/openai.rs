// openai.rs — OpenAI (Responses API) as a chat provider, plus the two things that come
// with putting anonymous users behind our one API key: a moderation prefilter and a
// per-session strike counter.
//
// Differences from Gemini that shape this file (docs/providers-openai.md):
//   - no free tier: every token is paid, the account is prepaid → all models `paid`;
//   - reasoning models: reasoning tokens are billed as OUTPUT and can take a minute —
//     `max_output_tokens` covers answer + reasoning, the catalog advertises a longer
//     `timeoutMs`, and the app's thinking slider maps to `reasoning.effort`;
//   - prompt caching is automatic (`input_tokens_details.cached_tokens` → cached rate);
//   - web search is a tool billed per call (no monthly free allowance like Gemini);
//   - `store: false` so nothing is kept in OpenAI's response store; inputs are still
//     retained ~30 days for abuse monitoring unless the org has Zero Data Retention
//     (SCRAI_OPENAI_RETENTION_DAYS tells the app what to show);
//   - `safety_identifier`: a per-session, per-day hash so OpenAI can act on ONE user's
//     abuse instead of throttling the whole org key. It never identifies a person and
//     never links days.
//
// Verified against developers.openai.com on 2026-09-03: /v1/responses with `input`
// content parts (`input_text` / `input_image` / `input_file`), `reasoning.effort`,
// `tools: [{type: "web_search"}]` → `web_search_call` output items, usage =
// {input_tokens, input_tokens_details.cached_tokens, output_tokens,
// output_tokens_details.reasoning_tokens}; /v1/moderations with `omni-moderation-latest`
// (free) → {flagged, categories}.

use scrai_core::billing::TokenUsage;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

const RESPONSES_URL: &str = "https://api.openai.com/v1/responses";
const MODERATIONS_URL: &str = "https://api.openai.com/v1/moderations";
/// Reasoning answers can take a minute or two; the default 120 s client would cut them.
pub const TIMEOUT_MS: u64 = 180_000;

/// Is this model id served by OpenAI? `gpt-oss-*` are OpenAI's OPEN-WEIGHT models,
/// which we do not host — they are served by other providers, so they are not ours.
pub fn is_openai_model(model: &str) -> bool {
    if model.starts_with("gpt-oss") || model.contains('/') {
        return false;
    }
    if model.starts_with("gpt-") {
        return true;
    }
    ["o1", "o3", "o4"].iter().any(|p| {
        model.starts_with(p) && model[p.len()..].chars().next().is_none_or(|c| c == '-')
    })
}

pub fn api_key() -> Result<String, String> {
    std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .ok_or_else(|| "OPENAI_API_KEY not set".to_string())
}

/// Days of retention to show in the app's privacy badge: 30 by default (OpenAI's abuse
/// monitoring window); 0 once the org has Zero Data Retention.
pub fn retention_days() -> u64 {
    std::env::var("SCRAI_OPENAI_RETENTION_DAYS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(30)
}

/// What one web-search tool call costs us (USD). OpenAI lists $10 / 1k calls for the
/// reasoning models we offer (a $25 preview rate exists for non-reasoning models).
pub fn search_usd_per_call() -> f64 {
    std::env::var("SCRAI_OPENAI_SEARCH_USD").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0.01)
}

/// MODERATION_PREFILTER=1 (default): run the free moderation endpoint on the user's
/// latest turn before the model call, and decline flagged input ourselves.
pub fn prefilter_enabled() -> bool {
    std::env::var("MODERATION_PREFILTER").map(|v| v.trim() != "0").unwrap_or(true)
}

/// The app's thinking budget (tokens) → `reasoning.effort`. Every current model accepts
/// low/medium/high; the newer "minimal"/"none" values are not universal, so the floor
/// stays "low".
pub fn effort_for(thinking: u64) -> &'static str {
    if thinking <= 1024 {
        "low"
    } else if thinking <= 8192 {
        "medium"
    } else {
        "high"
    }
}

/// Days since the epoch — the rotation period of the safety identifier and the strike
/// counter ("today" in UTC).
pub fn day_number() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0)
}

fn salt() -> &'static str {
    static S: OnceLock<String> = OnceLock::new();
    S.get_or_init(|| {
        std::env::var("SCRAI_ABUSE_SALT").ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| {
            // No salt configured → a fresh random one per boot (identifiers then also
            // rotate on restart, which is fine).
            use rand::RngCore;
            let mut b = [0u8; 16];
            rand::thread_rng().fill_bytes(&mut b);
            hex::encode(b)
        })
    })
}

/// `safety_identifier` for OpenAI: sha256(salt ‖ day ‖ session) — stable within a UTC
/// day so OpenAI can attribute one user's abuse, different tomorrow, never the raw id.
pub fn safety_identifier(session_id: &str, day: u64) -> String {
    let mut h = Sha256::new();
    h.update(salt().as_bytes());
    h.update(b"|");
    h.update(day.to_string().as_bytes());
    h.update(b"|");
    h.update(session_id.as_bytes());
    hex::encode(h.finalize())[..24].to_string()
}

// ---- abuse strikes -----------------------------------------------------------------

/// ABUSE_STRIKES_PER_DAY (default 3): declines ("Declined by …", incl. the moderation
/// prefilter) a session may collect per UTC day AT ONE PROVIDER before that provider's
/// models are refused to the session until the next day. Scoped per provider: three
/// OpenAI declines pause OpenAI, Gemini stays usable, and vice versa. The session's
/// balance stays where it is — the only "penalty" an anonymous session can carry.
pub fn strikes_per_day() -> u32 {
    std::env::var("ABUSE_STRIKES_PER_DAY").ok().and_then(|v| v.trim().parse().ok()).filter(|n| *n > 0).unwrap_or(3)
}

/// (session, provider) → (day, strikes today).
fn strikes() -> &'static Mutex<HashMap<(String, &'static str), (u64, u32)>> {
    static S: OnceLock<Mutex<HashMap<(String, &'static str), (u64, u32)>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record one decline at `provider` for the session today; returns today's count there.
pub fn strike(session_id: &str, provider: &'static str, day: u64) -> u32 {
    let mut m = strikes().lock().unwrap_or_else(|e| e.into_inner());
    // Drop yesterday's entries on the way past (bounded growth; strikes are daily).
    m.retain(|_, (d, _)| *d == day);
    let e = m.entry((session_id.to_string(), provider)).or_insert((day, 0));
    e.1 += 1;
    eprintln!(
        "scrai-server: ABUSE strike {}/{} at {provider} for session {}… (day {day})",
        e.1,
        strikes_per_day(),
        &session_id[..session_id.len().min(8)]
    );
    e.1
}

/// Has the session used up today's strikes at this provider?
pub fn blocked(session_id: &str, provider: &'static str, day: u64) -> bool {
    strikes()
        .lock()
        .map(|m| {
            m.get(&(session_id.to_string(), provider))
                .is_some_and(|(d, n)| *d == day && *n >= strikes_per_day())
        })
        .unwrap_or(false)
}

// ---- request / response shapes ------------------------------------------------------

/// Our OpenAI-shaped messages (`{role, content, attachments?}`) → Responses `input`.
/// Images become `input_image` data URLs, PDFs `input_file`; other attachment types are
/// named but not sent (the model can't read them anyway).
pub fn to_input(messages: &Value) -> Value {
    let empty = Vec::new();
    let items: Vec<Value> = messages
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .map(|m| {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let text = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
            let atts = m.get("attachments").and_then(|a| a.as_array());
            match (role, atts) {
                // Assistant turns are plain text (no attachments on our side).
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
                        } else {
                            parts.push(json!({ "type": "input_text", "text": format!("[attachment {} of type {mime} omitted]", i + 1) }));
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

/// All `output_text` parts of the answer, joined.
pub fn output_text(j: &Value) -> String {
    let empty = Vec::new();
    j.get("output")
        .and_then(|o| o.as_array())
        .unwrap_or(&empty)
        .iter()
        .filter(|item| item.get("type").and_then(|t| t.as_str()) == Some("message"))
        .flat_map(|item| item.get("content").and_then(|c| c.as_array()).cloned().unwrap_or_default())
        .filter(|part| part.get("type").and_then(|t| t.as_str()) == Some("output_text"))
        .filter_map(|part| part.get("text").and_then(|t| t.as_str()).map(str::to_string))
        .collect::<Vec<_>>()
        .join("")
}

/// Completed web-search tool calls in the answer (each one is billed).
pub fn search_calls(j: &Value) -> u64 {
    let empty = Vec::new();
    j.get("output")
        .and_then(|o| o.as_array())
        .unwrap_or(&empty)
        .iter()
        .filter(|item| item.get("type").and_then(|t| t.as_str()) == Some("web_search_call"))
        .filter(|item| item.get("status").and_then(|s| s.as_str()).is_none_or(|s| s == "completed"))
        .count() as u64
}

/// OpenAI's usage → ours. `output_tokens` already includes reasoning; `cached_tokens` are
/// the part of the input billed at the cached rate (they are counted inside
/// `input_tokens`, so we subtract them like the Gemini adapter does).
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

/// The user's latest turn as moderation input (text + images).
fn last_user_turn(messages: &Value) -> Option<Value> {
    let m = messages.as_array()?.iter().rev().find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))?;
    let text = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
    let mut parts = vec![json!({ "type": "text", "text": text })];
    if let Some(atts) = m.get("attachments").and_then(|a| a.as_array()) {
        for att in atts {
            let mime = att.get("mimeType").and_then(|x| x.as_str()).unwrap_or("");
            let data = att.get("data").and_then(|x| x.as_str()).unwrap_or("");
            if mime.starts_with("image/") {
                parts.push(json!({ "type": "image_url", "image_url": { "url": format!("data:{mime};base64,{data}") } }));
            }
        }
    }
    Some(Value::Array(parts))
}

/// Run the (free) moderation endpoint on the latest user turn. `Ok(Some(categories))`
/// = flagged, `Ok(None)` = clean. A moderation outage fails OPEN with a log line: the
/// model's own policy check still stands behind it.
pub async fn moderation_flagged(messages: &Value) -> Result<Option<String>, String> {
    let Some(input) = last_user_turn(messages) else { return Ok(None) };
    let key = api_key()?;
    let res = crate::http::client()
        .post(MODERATIONS_URL)
        .bearer_auth(key)
        .timeout(std::time::Duration::from_secs(20))
        .json(&json!({ "model": "omni-moderation-latest", "input": input }))
        .send()
        .await;
    let j: Value = match res {
        Ok(r) if r.status().is_success() => r.json().await.unwrap_or(Value::Null),
        Ok(r) => {
            eprintln!("scrai-server: openai moderation {} — skipping the prefilter for this request", r.status());
            return Ok(None);
        }
        Err(e) => {
            eprintln!("scrai-server: openai moderation unreachable ({e}) — skipping the prefilter for this request");
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

/// One chat turn. `session_id` feeds the safety identifier (None on the free tier —
/// no OpenAI model is free, so in practice always Some).
pub async fn chat(
    model: &str,
    messages: &Value,
    max_tokens: u64,
    live: bool,
    thinking: u64,
    session_id: Option<&str>,
) -> Result<(String, TokenUsage), String> {
    let key = api_key()?;
    if prefilter_enabled() {
        if let Some(cats) = moderation_flagged(messages).await? {
            return Err(format!("Declined by OpenAI (moderation: {cats})"));
        }
    }
    let mut body = json!({
        "model": model,
        "input": to_input(messages),
        "max_output_tokens": max_tokens + thinking,
        "reasoning": { "effort": effort_for(thinking) },
        // Never keep the exchange in OpenAI's response store.
        "store": false,
    });
    if live {
        body["tools"] = json!([{ "type": "web_search" }]);
    }
    if let Some(sid) = session_id {
        body["safety_identifier"] = json!(safety_identifier(sid, day_number()));
    }
    let res = crate::http::client()
        .post(RESPONSES_URL)
        .bearer_auth(key)
        .timeout(std::time::Duration::from_millis(TIMEOUT_MS))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("openai request failed: {e}"))?;
    let status = res.status();
    let retry_after = res
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let j: Value = res.json().await.map_err(|e| format!("openai returned non-JSON ({status}): {e}"))?;
    if !status.is_success() {
        let code = j.pointer("/error/code").and_then(|c| c.as_str()).unwrap_or("");
        let msg = j.pointer("/error/message").and_then(|m| m.as_str()).unwrap_or("unknown error");
        return Err(match status.as_u16() {
            429 | 502 | 503 | 504 => {
                eprintln!("scrai-server: openai {status} (retry-after {retry_after:?}): {}", msg.chars().take(200).collect::<String>());
                match retry_after {
                    Some(n) => format!("OpenAI is limiting requests right now — please try again in about {n} seconds"),
                    None => "OpenAI is busy right now — please try again in a minute".into(),
                }
            }
            400 | 403 if code.contains("policy") || msg.to_lowercase().contains("policy") || msg.to_lowercase().contains("safety") => {
                format!("Declined by OpenAI (policy): {}", msg.chars().take(160).collect::<String>())
            }
            400 if code == "context_length_exceeded" || msg.contains("context length") || msg.contains("too many tokens") => {
                "the conversation is too long for this model — start a new chat or prune the history".into()
            }
            401 => "OpenAI rejected the API key — check OPENAI_API_KEY".into(),
            _ => format!("openai {status}: {}", msg.chars().take(200).collect::<String>()),
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
    Ok((text, usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_routing_recognises_openai_ids_only() {
        assert!(is_openai_model("gpt-5.4-mini"));
        assert!(is_openai_model("o3-mini"));
        assert!(is_openai_model("o3"));
        assert!(!is_openai_model("gpt-oss-120b"));
        assert!(!is_openai_model("openai/gpt-oss-120b"));
        assert!(!is_openai_model("gemini-3.5-flash-lite"));
        assert!(!is_openai_model("o4x"));
    }

    #[test]
    fn effort_follows_the_thinking_slider() {
        assert_eq!(effort_for(0), "low");
        assert_eq!(effort_for(1024), "low");
        assert_eq!(effort_for(4096), "medium");
        assert_eq!(effort_for(16_000), "high");
    }

    #[test]
    fn usage_and_text_parse_from_a_responses_reply() {
        let j = json!({
            "output": [
                { "type": "web_search_call", "status": "completed", "action": { "type": "search" } },
                { "type": "message", "content": [ { "type": "output_text", "text": "Hello " }, { "type": "output_text", "text": "world" } ] }
            ],
            "usage": { "input_tokens": 120, "input_tokens_details": { "cached_tokens": 100 },
                       "output_tokens": 1186, "output_tokens_details": { "reasoning_tokens": 1024 }, "total_tokens": 1306 }
        });
        assert_eq!(output_text(&j), "Hello world");
        let u = parse_usage(&j);
        assert_eq!((u.input, u.cached_input, u.output, u.grounding_queries), (20, 100, 1186, 1));
        assert!(!u.estimated);
    }

    #[test]
    fn input_conversion_keeps_roles_and_maps_attachments() {
        let msgs = json!([
            { "role": "system", "content": "be brief" },
            { "role": "user", "content": "look", "attachments": [
                { "mimeType": "image/png", "data": "AAAA" },
                { "mimeType": "application/pdf", "data": "BBBB" },
                { "mimeType": "audio/ogg", "data": "CCCC" } ] },
            { "role": "assistant", "content": "ok" }
        ]);
        let inp = to_input(&msgs);
        assert_eq!(inp[0]["content"], "be brief");
        assert_eq!(inp[1]["content"][1]["type"], "input_image");
        assert_eq!(inp[1]["content"][1]["image_url"], "data:image/png;base64,AAAA");
        assert_eq!(inp[1]["content"][2]["type"], "input_file");
        assert!(inp[1]["content"][3]["text"].as_str().unwrap().contains("omitted"));
        assert_eq!(inp[2]["content"], "ok");
    }

    #[test]
    fn safety_identifier_is_stable_per_day_and_never_the_raw_session() {
        let a = safety_identifier("session-abc", 20_000);
        assert_eq!(a, safety_identifier("session-abc", 20_000));
        assert_ne!(a, safety_identifier("session-abc", 20_001));
        assert_ne!(a, safety_identifier("session-xyz", 20_000));
        assert_eq!(a.len(), 24);
        assert!(!a.contains("session"));
    }

    #[test]
    fn strikes_block_one_provider_for_the_day_and_reset_tomorrow() {
        let n = strikes_per_day();
        for i in 1..n {
            assert_eq!(strike("s-block", "openai", 500), i);
            assert!(!blocked("s-block", "openai", 500));
        }
        strike("s-block", "openai", 500);
        assert!(blocked("s-block", "openai", 500));
        assert!(!blocked("s-block", "gemini", 500)); // other provider unaffected
        assert!(!blocked("s-block", "openai", 501)); // a new day
        assert!(!blocked("s-other", "openai", 500));
    }
}
