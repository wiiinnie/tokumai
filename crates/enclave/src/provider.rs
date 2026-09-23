//! The model providers, and the request shape they all take.
//!
//! Messages keep the shape the first app sent — `{role, content, attachments?}` with
//! attachments as `{mimeType, data}` (base64) — so the app's history carries over as is.
//! Each provider translates it into its own API.

use crate::policy;
use crate::secrets::SecretSource;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use tokumai_core::billing::TokenUsage;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Value,
    #[serde(default, rename = "maxTokens")]
    pub max_tokens: Option<u64>,
    #[serde(default, rename = "thinkingBudget")]
    pub thinking: Option<u64>,
    /// Let the model search the web while answering (billed per query).
    #[serde(default)]
    pub live: bool,
    #[serde(default, rename = "imageSize")]
    pub image_size: Option<String>,
    /// Keep a generated picture pixel for pixel (see `picture`): slower, several times
    /// the bytes over the mixnet. Off by default.
    #[serde(default)]
    pub lossless: bool,
}

impl ChatRequest {
    pub fn answer_tokens(&self) -> u64 {
        self.max_tokens.unwrap_or(policy::DEFAULT_MAX_TOKENS).clamp(1, policy::MAX_OUTPUT_TOKENS)
    }
    pub fn thinking_tokens(&self) -> u64 {
        self.thinking.unwrap_or(policy::DEFAULT_THINKING).min(policy::MAX_THINKING)
    }
}

/// One call as a provider sees it: the request, plus what the enclave adds.
pub struct Call<'a> {
    pub req: &'a ChatRequest,
    /// A per-account, per-day pseudonym for providers that act on one user's abuse
    /// (OpenAI's `safety_identifier`). Never the account id; different tomorrow.
    pub safety_id: Option<String>,
    /// The picture size actually requested (see `gemini::effective_image_size`).
    pub image_size: &'static str,
}

#[derive(Debug, Clone)]
pub struct Completion {
    pub text: String,
    pub usage: TokenUsage,
    /// Generated pictures, `[{mimeType, data}]`.
    pub images: Option<Value>,
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait Provider: Send + Sync {
    /// "openai", "gemini" — the key strikes are counted under.
    fn name(&self) -> &'static str;
    fn serves(&self, model: &str) -> bool;
    fn complete<'a>(&'a self, call: &'a Call<'a>) -> BoxFuture<'a, Result<Completion, String>>;
}

/// Every configured provider, and the moderation check that runs before any of them.
pub struct Providers {
    list: Vec<Box<dyn Provider>>,
    moderation: Option<crate::openai::Moderation>,
}

impl Providers {
    /// The real providers, for whichever keys exist. No key, no provider: its models are
    /// simply not offered.
    pub fn from_secrets(secrets: &dyn SecretSource) -> Providers {
        let mut list: Vec<Box<dyn Provider>> = Vec::new();
        let openai_key = secrets.get("OPENAI_API_KEY");
        if let Some(k) = &openai_key {
            list.push(Box::new(crate::openai::OpenAi { key: k.clone() }));
        }
        if let Some(k) = secrets.get("GEMINI_API_KEY") {
            list.push(Box::new(crate::gemini::Gemini { key: k }));
        }
        // The moderation check is OpenAI's (free, covers text and images) and runs in front
        // of EVERY provider: Gemini has no per-user pseudonym, so what it refuses lands on
        // the whole key — better to stop it before it gets there.
        let moderation = openai_key.map(|key| crate::openai::Moderation { key });
        Providers { list, moderation }
    }

    /// Development: the mock alone.
    /// One call that looks like an ordinary one from outside, and is thrown away. It is
    /// cover for somebody else's first question (see `cover`), so what matters is the
    /// shape: a few kilobytes back, or several megabytes. Failure is silence — a decoy
    /// that did not go out is a missed opportunity, never an error for the person waiting.
    pub async fn decoy(&self, picture: bool) {
        let model = if picture { "gemini-3.1-flash-lite-image" } else { "gemini-3.5-flash-lite" };
        let Some(provider) = self.find(model) else { return };
        // Harmless, and long enough that the answer is of an ordinary size.
        let req = ChatRequest {
            model: model.to_string(),
            messages: serde_json::json!([{ "role": "user", "content": if picture { "a plain grey square" } else { "Name three colours and say nothing else." } }]),
            max_tokens: Some(if picture { 64 } else { 256 }),
            ..Default::default()
        };
        let call = Call { req: &req, safety_id: None, image_size: "1K" };
        let _ = provider.complete(&call).await;
    }

    pub fn mock() -> Providers {
        Providers { list: vec![Box::new(MockProvider)], moderation: None }
    }

    /// Development: the mock beside whatever real providers are configured.
    pub fn with_mock(mut self) -> Providers {
        self.list.push(Box::new(MockProvider));
        self
    }

    /// Tests: exactly these providers, no moderation.
    pub fn with(list: Vec<Box<dyn Provider>>) -> Providers {
        Providers { list, moderation: None }
    }

    pub fn find(&self, model: &str) -> Option<&dyn Provider> {
        self.list.iter().find(|p| p.serves(model)).map(|p| p.as_ref())
    }

    /// `Ok(Some(categories))` when the latest user turn is flagged.
    pub async fn moderate(&self, messages: &Value) -> Result<Option<String>, String> {
        match &self.moderation {
            Some(m) => m.flagged(messages).await,
            None => Ok(None),
        }
    }
}

/// Development: echoes, and reports token counts the way a provider would.
pub struct MockProvider;

impl Provider for MockProvider {
    fn name(&self) -> &'static str {
        "mock"
    }
    fn serves(&self, model: &str) -> bool {
        model == "mock"
    }
    fn complete<'a>(&'a self, call: &'a Call<'a>) -> BoxFuture<'a, Result<Completion, String>> {
        Box::pin(async move {
            let msgs = call.req.messages.as_array().cloned().unwrap_or_default();
            let last = msgs.last().and_then(|m| m.get("content")).and_then(|c| c.as_str()).unwrap_or("").to_string();
            let text = format!("(mock) you said: {last}");
            let input: u64 = msgs
                .iter()
                .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
                .map(|c| tokumai_core::billing::estimate_tokens(c.len() as u64))
                .sum();
            let output = tokumai_core::billing::estimate_tokens(text.len() as u64).min(call.req.answer_tokens());
            Ok(Completion { text, usage: TokenUsage { input, output, ..Default::default() }, images: None })
        })
    }
}
