//! The model providers. The enclave talks to them over TLS from inside, so the host sees only
//! that a request went out and how big it was. The real adapters (OpenAI, Gemini) come over
//! from the first server; development runs against [`MockProvider`].

use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

fn default_max_tokens() -> u32 {
    1024
}

#[derive(Debug, Clone)]
pub struct Completion {
    pub text: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait Provider: Send + Sync {
    fn complete<'a>(&'a self, req: &'a ChatRequest) -> BoxFuture<'a, Result<Completion, String>>;
}

/// A rough token count for text: a quarter of its characters, at least one.
pub fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64 / 4).max(1)
}

/// Development: answers by echoing, and reports token counts the way a provider would.
pub struct MockProvider;

impl Provider for MockProvider {
    fn complete<'a>(&'a self, req: &'a ChatRequest) -> BoxFuture<'a, Result<Completion, String>> {
        Box::pin(async move {
            let last = req.messages.last().map(|m| m.content.as_str()).unwrap_or("");
            let text = format!("(mock {}) you said: {last}", req.model);
            let input = req.messages.iter().map(|m| estimate_tokens(&m.content)).sum();
            let output = estimate_tokens(&text).min(req.max_tokens as u64);
            Ok(Completion { text, input_tokens: input, output_tokens: output })
        })
    }
}
