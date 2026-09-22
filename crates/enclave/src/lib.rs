//! The tokumai enclave: everything that must not be visible to whoever runs the machine.
//!
//! What the operator's host sees is ciphertext in and ciphertext out, plus the fact that a
//! request went to a model provider. Inside, and only inside, an account is matched with its
//! balance and its question.
//!
//! The pieces that differ between a developer's laptop and a real enclave are traits, chosen
//! once in [`Platform`]:
//! - [`tokumai_attest::Attester`] — who vouches for the code (simulator, AWS Nitro, Google);
//! - [`seal::KeyProvider`] — where the data key comes from (a local file, or a KMS that
//!   releases it only to attested code);
//! - [`provider::Providers`] — the model providers (a mock, or OpenAI and Gemini), with
//!   their keys from a [`secrets::SecretSource`] (the environment, or sealed secrets).
//!
//! Everything else — the ledger, the billing, and the wire format (`tokumai-proto`, shared
//! with the app) — is the same in both.

pub mod apple;
pub mod catalog;
pub mod gemini;
pub mod http;
pub mod keys;
pub mod kms;
pub mod ledger;
pub mod openai;
pub mod plans;
pub mod picture;
pub mod policy;
pub mod provider;
pub mod seal;
pub mod secrets;
pub mod secrets_sealed;
pub mod service;
pub mod state;
pub mod stripe;
pub mod subscriptions;

pub use service::{Enclave, Platform};

/// Milliseconds since the epoch. The one clock the enclave reads.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}
