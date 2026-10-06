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

pub mod admin;
pub mod apple;
pub mod catalog;
pub mod cover;
pub mod doors;
pub mod gemini;
pub mod ghost;
pub mod http;
pub mod keys;
pub mod kms;
pub mod ledger;
pub mod notes;
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
pub mod witness;

pub use service::{Enclave, Platform};

/// A development-only account of what the enclave is doing, for the one question an
/// operator cannot otherwise answer: did the request arrive at all?
///
/// "It hangs" has three quite different causes — the request never arrived, it arrived and
/// is stuck, or it was answered and the answer was lost on the way back — and from outside
/// all three look identical. Two days went into telling them apart by guessing.
///
/// What a line carries: the operation's NAME, the kind of answer, and the milliseconds.
/// Never an account, never any content. It is a trace of the machine, not of a person.
/// Even so it is a trace of use, so the feature is off unless an image is built for
/// testing, and PCR0 says which image is running. It comes out before mainnet
/// (docs/terms-notes.md).
pub mod trace {
    use std::sync::OnceLock;

    /// Compiled in only with the feature; every call below is a no-op otherwise, and the
    /// formatting goes with it.
    pub const ON: bool = cfg!(feature = "trace-requests");

    static VOICE: OnceLock<fn(String)> = OnceLock::new();

    /// The enclave has no console of its own; the binary lends it one (`server::say`).
    pub fn speaks(f: fn(String)) {
        let _ = VOICE.set(f);
    }

    pub fn say(line: impl FnOnce() -> String) {
        if ON {
            if let Some(f) = VOICE.get() {
                f(line());
            }
        }
    }
}

/// What the enclave says about itself to whoever runs it: a line for the host log, with
/// nothing of a person in it. The book's writer uses it when the host will not take a
/// record. Unlike `trace`, always compiled in: these are the lines an operator needs in
/// order to understand a silence, not a trace of use.
pub mod voice {
    use std::sync::OnceLock;

    static VOICE: OnceLock<fn(String)> = OnceLock::new();

    /// The enclave has no console of its own; the binary lends it one. The function must
    /// be safe to call from any thread, inside a runtime or not.
    pub fn speaks(f: fn(String)) {
        let _ = VOICE.set(f);
    }

    pub fn say(line: String) {
        match VOICE.get() {
            Some(f) => f(line),
            None => eprintln!("{line}"),
        }
    }
}

/// Milliseconds since the epoch. The one clock the enclave reads.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}
