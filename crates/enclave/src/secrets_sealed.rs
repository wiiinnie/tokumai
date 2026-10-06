//! The secrets an enclave runs on, as they arrive: sealed by KMS, opened inside.
//!
//! The file on the host is a KMS blob; only an enclave whose attestation shows an allowed
//! image can have it decrypted (`kms`). What is inside is a small JSON object — the
//! provider keys and the Stripe keys, which the operator knows anyway — and it never
//! touches the host's disk in the clear.
//!
//! What is NOT in it, since 2026-10-05: the data key and the doors' Nym identities. Those
//! are the enclave's own and the operator must never hold them (the data key names every
//! account and every payment in the book; a door's identity is its address). The data key
//! is born in the enclave (`kms::generate_data_key_to_enclave`), the identities are made by
//! the Nym client and kept sealed under that key (`doors`). A sealed file that still carries
//! a `dataKey` is refused outright — see `Sealed::check` — because an enclave that would
//! accept a key the operator chose is an enclave the operator can read.

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

    /// Refuse what must not come from outside. A `dataKey` in the sealed file is the
    /// operator naming the key the book is written under — the one thing the whole
    /// arrangement exists to prevent — so it is not read, not even to migrate.
    pub fn check(&self) -> Result<(), String> {
        if self.0.get("dataKey").is_some() {
            return Err("the sealed secrets carry a dataKey — the data key is born in the enclave since 2026-10-05 and must not be chosen outside it; remove it and seal again (deploy/aws/kms.sh secrets refuses it)".into());
        }
        Ok(())
    }

    /// The doors' identities the operator sealed, if any: `nymIdentities: { "<gateway>":
    /// { "<file>": "<base64>" } }` (or one `nymIdentity` for every door). Read ONCE, by an
    /// enclave that finds no sealed doors of its own on the host, so that the addresses the
    /// apps pin survive the move; from then on the enclave keeps them itself (`doors`) and
    /// the operator takes them out of the file. An identity the operator has held is an
    /// address the operator could stand up elsewhere, so each of these should be rotated
    /// before launch — the enclave says so out loud when it takes one.
    pub fn operator_held_doors(&self) -> Option<serde_json::Map<String, Value>> {
        let all = self.0["nymIdentities"].as_object().cloned();
        let one = self.0["nymIdentity"].as_object().cloned();
        match (all, one) {
            (Some(all), _) if !all.is_empty() => Some(all),
            (_, Some(one)) if !one.is_empty() => {
                let mut m = serde_json::Map::new();
                m.insert("*".to_string(), Value::Object(one));
                Some(m)
            }
            _ => None,
        }
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
        let s = Sealed::open(br#"{"OPENAI_API_KEY":"sk-x","empty":""}"#).unwrap();
        s.check().unwrap();
        assert_eq!(s.get("OPENAI_API_KEY").as_deref(), Some("sk-x"));
        assert_eq!(s.get("empty"), None);
        assert_eq!(s.get("GEMINI_API_KEY"), None);
        assert!(Sealed::open(b"[]").is_err());
    }

    /// The operator does not get to choose the key the book is written under.
    #[test]
    fn a_data_key_from_outside_is_refused() {
        let s = Sealed::open(br#"{"dataKey":"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff","OPENAI_API_KEY":"sk-x"}"#).unwrap();
        assert!(s.check().unwrap_err().contains("dataKey"));
    }

    #[test]
    fn operator_held_doors_are_read_once_in_either_form() {
        let s = Sealed::open(br#"{"nymIdentities":{"gw1":{"private_identity.pem":"AA=="}}}"#).unwrap();
        assert_eq!(s.operator_held_doors().unwrap().len(), 1);
        let s = Sealed::open(br#"{"nymIdentity":{"private_identity.pem":"AA=="}}"#).unwrap();
        assert!(s.operator_held_doors().unwrap().contains_key("*"));
        let s = Sealed::open(br#"{"nymIdentities":{}}"#).unwrap();
        assert!(s.operator_held_doors().is_none());
        assert!(Sealed::open(b"{}").unwrap().operator_held_doors().is_none());
    }
}
