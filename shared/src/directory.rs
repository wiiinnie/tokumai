use serde::{Deserialize, Serialize};
use crate::Provider;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DirectoryRequest {
    ListServers {
        reply_to: Option<String>,
    },
    Register {
        address: String,
        version: String,
        providers: Vec<Provider>, // NEW: Available APIs
        reply_to: String,
    },
    ChallengeResponse {
        challenge_id: String,
        response: u32,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DirectoryResponse {
    ServerList {
        servers: Vec<ServerEntry>,
    },
    Challenge {
        challenge_id: String,
        number: u32,
        expires_at: String,
    },
    RegistrationSuccess {
        expires_at: String,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerEntry {
    pub address: String,
    pub version: String,
    pub providers: Vec<Provider>, // NEW: Available APIs
    pub registered_at: String,
    pub expires_at: String,
}
