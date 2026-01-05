use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Provider {
    Groq,
    OpenAI,
    Anthropic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Ping { 
        timestamp: u64,
        message: String,
        reply_to: Option<String>, // Client Nym address for fast mode
    },
    Chat(ChatRequest),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Pong { 
        original_timestamp: u64,
        server_timestamp: u64,
        message: String,
    },
    Chat(ChatResponse),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub provider: Provider,
    pub model: String,
    pub prompt: String,
    pub reply_to: Option<String>, // Client Nym address for fast mode
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub content: String,
    pub model: String,
    pub provider: Provider,
}

impl Request {
    pub fn ping(message: &str, reply_to: Option<String>) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        
        Self::Ping {
            timestamp,
            message: message.to_string(),
            reply_to,
        }
    }
    
    pub fn chat_groq(prompt: &str, reply_to: Option<String>) -> Self {
        Self::Chat(ChatRequest {
            provider: Provider::Groq,
            model: "llama-3.3-70b-versatile".to_string(),
            prompt: prompt.to_string(),
            reply_to,
        })
    }
    
    pub fn chat_openai(prompt: &str, reply_to: Option<String>) -> Self {
        Self::Chat(ChatRequest {
            provider: Provider::OpenAI,
            model: "gpt-4o".to_string(),
            prompt: prompt.to_string(),
            reply_to,
        })
    }
    
    pub fn chat_anthropic(prompt: &str, reply_to: Option<String>) -> Self {
        Self::Chat(ChatRequest {
            provider: Provider::Anthropic,
            model: "claude-3-5-sonnet-20241022".to_string(),
            prompt: prompt.to_string(),
            reply_to,
        })
    }
}

impl Response {
    pub fn pong(original_timestamp: u64, message: &str) -> Self {
        let server_timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        
        Self::Pong {
            original_timestamp,
            server_timestamp,
            message: message.to_string(),
        }
    }
}
