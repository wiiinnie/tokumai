//! The secrets an enclave runs on, as they arrive: sealed by KMS, opened inside.
//!
//! The file on the host is a KMS blob; only an enclave whose attestation shows an allowed
//! image can have it decrypted (`kms`). What is inside is a small JSON object — the
//! provider keys, the Stripe keys, the enclave's data key and its Nym identity — and it
//! never touches the host's disk in the clear.

use serde_json::Value;

/// The two parts of what the host keeps: the secrets under their own key
/// (ChaCha20-Poly1305) and that key sealed by KMS. KMS encrypts only small pieces, and the
/// secrets carry the Nym identity with them.
#[derive(serde::Deserialize)]
pub struct Envelope {
    /// The key, as KMS sealed it (base64).
    #[serde(rename = "kmsKey")]
    pub kms_key: String,
    nonce: String,
    ct: String,
}

impl Envelope {
    pub fn parse(text: &[u8]) -> Result<Envelope, String> {
        serde_json::from_slice(text).map_err(|e| format!("the sealed file is unreadable: {e}"))
    }

    /// Open the secrets with the key KMS gave back.
    pub fn open(&self, key: &[u8]) -> Result<Sealed, String> {
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        use chacha20poly1305::aead::{Aead, KeyInit};
        use chacha20poly1305::{ChaCha20Poly1305, Nonce};
        let nonce = B64.decode(self.nonce.trim()).map_err(|e| e.to_string())?;
        let ct = B64.decode(self.ct.trim()).map_err(|e| e.to_string())?;
        let plain = ChaCha20Poly1305::new_from_slice(key)
            .map_err(|_| "the key KMS returned is not 32 bytes".to_string())?
            .decrypt(Nonce::from_slice(&nonce), ct.as_slice())
            .map_err(|_| "the sealed secrets do not open with that key".to_string())?;
        Sealed::open(&plain)
    }
}

/// The opened secrets. Also a [`crate::secrets::SecretSource`], so everything that already
/// reads keys from the environment reads them from here instead.
pub struct Sealed(Value);

impl Sealed {
    /// Parse the opened JSON.
    pub fn open(plaintext: &[u8]) -> Result<Sealed, String> {
        let v: Value = serde_json::from_slice(plaintext).map_err(|e| format!("the sealed secrets are not JSON: {e}"))?;
        if !v.is_object() {
            return Err("the sealed secrets are not an object".into());
        }
        Ok(Sealed(v))
    }

    /// The enclave's data key (`dataKey`, 32 bytes hex): what the ledger and everything
    /// else at rest is keyed with.
    pub fn data_key(&self) -> Result<[u8; 32], String> {
        let hex = self.0["dataKey"].as_str().ok_or("the sealed secrets carry no dataKey")?;
        let bytes = hex::decode(hex.trim()).map_err(|e| format!("dataKey is not hex: {e}"))?;
        bytes.try_into().map_err(|_| "dataKey is not 32 bytes".to_string())
    }

    /// The enclave's Nym identity, if one was sealed with it: the files of its client
    /// store, base64 by name — so the enclave keeps its address across restarts.
    pub fn nym_identity(&self) -> Option<&serde_json::Map<String, Value>> {
        self.0["nymIdentity"].as_object()
    }
}

impl crate::secrets::SecretSource for Sealed {
    fn get(&self, name: &str) -> Option<String> {
        self.0[name].as_str().map(str::to_string).filter(|v| !v.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::SecretSource;

    #[test]
    fn what_was_sealed_comes_back_named() {
        let s = Sealed::open(br#"{"dataKey":"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff","OPENAI_API_KEY":"sk-x","empty":""}"#).unwrap();
        assert_eq!(s.data_key().unwrap()[..4], [0x00, 0x11, 0x22, 0x33]);
        assert_eq!(s.get("OPENAI_API_KEY").as_deref(), Some("sk-x"));
        assert_eq!(s.get("empty"), None);
        assert_eq!(s.get("GEMINI_API_KEY"), None);
        assert!(Sealed::open(br#"{"dataKey":"tooshort"}"#).unwrap().data_key().is_err());
        assert!(Sealed::open(b"[]").is_err());
    }
}
