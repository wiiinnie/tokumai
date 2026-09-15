// keys.rs — a throw-away account / session signer, byte-compatible with the app's
// account.rs (SPKI-PEM Ed25519, id = sha256(pem), account sig over
// `accountId:purpose:nonce`, session sig over `sessionId:counter:sha256hex(body)`).
// The harness never needs a mnemonic: the server derives every id from the PEM alone.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ed25519_dalek::{Signer as _, SigningKey};
use rand::RngCore;

/// ASN.1 SPKI prefix for an Ed25519 public key; the 32 key bytes follow.
const SPKI_ED25519_PREFIX: [u8; 12] =
    [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];

pub struct Signer {
    sk: SigningKey,
    /// Exactly the PEM Node / the app produce (one 60-char base64 line).
    pub pem: String,
    /// sha256(trimmed PEM) hex — the account id or session id the server sees.
    pub id: String,
}

impl Signer {
    pub fn random() -> Self {
        let mut seed = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut seed);
        let sk = SigningKey::from_bytes(&seed);
        let mut der = Vec::with_capacity(44);
        der.extend_from_slice(&SPKI_ED25519_PREFIX);
        der.extend_from_slice(&sk.verifying_key().to_bytes());
        let pem = format!("-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n", B64.encode(der));
        let id = scrai_core::auth::id_for(&pem);
        Self { sk, pem, id }
    }

    /// Prove account ownership (invoice / entitlement / withdraw).
    pub fn sign_account(&self, purpose: &str, nonce: &str) -> String {
        let msg = format!("{}:{}:{}", self.id, purpose, nonce);
        B64.encode(self.sk.sign(msg.as_bytes()).to_bytes())
    }

}

pub fn rand_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The server's verifier must accept what the harness signs — same contract the
    /// app relies on (core::auth is the one source of truth for the byte format).
    #[test]
    fn server_side_verifier_accepts_harness_signatures() {
        let s = Signer::random();
        let sig = s.sign_account("invoice:5", "n1");
        assert_eq!(scrai_core::auth::account_owns(&s.pem, "invoice:5", "n1", &sig), Some(s.id.clone()));
    }
}
