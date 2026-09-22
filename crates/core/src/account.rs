//! An account is an Ed25519 key derived from a 24-word recovery phrase. Its id is the sha256
//! of the key's SPKI-PEM, which is how the first tokumai server named accounts, so the same
//! phrase opens the same account here.
//!
//! Every request an account makes is signed over `accountId:purpose:nonce`, the same shape as
//! before. The enclave adds its own identity and a hash of the request body into the nonce, so a
//! signature is good for exactly one request to exactly one enclave.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use bip39::Mnemonic;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

/// ASN.1 SPKI prefix for an Ed25519 public key; the 32 key bytes follow.
const SPKI_ED25519_PREFIX: [u8; 12] = [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];

pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// The PEM Node's `export({type:'spki',format:'pem'})` produces — kept so ids match.
pub fn spki_pem(pubkey: &[u8; 32]) -> String {
    let mut der = Vec::with_capacity(44);
    der.extend_from_slice(&SPKI_ED25519_PREFIX);
    der.extend_from_slice(pubkey);
    format!("-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n", B64.encode(der))
}

/// sha256 of the trimmed PEM, hex — the account's name.
pub fn id_for(pem: &str) -> String {
    hex::encode(sha256(&[pem.trim().as_bytes()]))
}

pub struct Account {
    pub mnemonic: String,
    signing: SigningKey,
    pub public_key_pem: String,
    pub account_id: String,
}

/// A fresh account, as 24 words.
pub fn create_account() -> Account {
    let m = Mnemonic::generate_in(bip39::Language::English, 24).expect("mnemonic generation");
    from_mnemonic(&m.to_string()).expect("a freshly generated mnemonic is valid")
}

/// Rebuild an account from its phrase. Same words in, same keys out.
pub fn from_mnemonic(phrase: &str) -> Result<Account, String> {
    let norm = phrase.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    let m = Mnemonic::parse_in_normalized(bip39::Language::English, &norm)
        .map_err(|_| "that is not a valid recovery phrase — check the words and their order".to_string())?;
    let seed = m.to_seed("");
    // The label is the first server's; changing it would give every phrase a new account.
    let signing = SigningKey::from_bytes(&sha256(&[b"scrai/account/v1", &seed]));
    let pem = spki_pem(&signing.verifying_key().to_bytes());
    let account_id = id_for(&pem);
    Ok(Account { mnemonic: norm, signing, public_key_pem: pem, account_id })
}

impl Account {
    /// Sign `accountId:purpose:nonce`, base64.
    pub fn sign(&self, purpose: &str, nonce: &str) -> String {
        let msg = format!("{}:{}:{}", self.account_id, purpose, nonce);
        B64.encode(self.signing.sign(msg.as_bytes()).to_bytes())
    }
}

fn verifying_key(pem: &str) -> Option<VerifyingKey> {
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    let der = B64.decode(body.trim()).ok()?;
    let raw = der.strip_prefix(&SPKI_ED25519_PREFIX[..])?;
    VerifyingKey::from_bytes(&raw.try_into().ok()?).ok()
}

/// Did the holder of this account key sign `accountId:purpose:nonce`? Returns the account id.
/// Replay protection is the caller's: consume the nonce only after this returns Some.
pub fn account_owns(public_key_pem: &str, purpose: &str, nonce: &str, sig_b64: &str) -> Option<String> {
    let account_id = id_for(public_key_pem);
    let key = verifying_key(public_key_pem)?;
    let sig = Signature::from_slice(&B64.decode(sig_b64).ok()?).ok()?;
    let msg = format!("{account_id}:{purpose}:{nonce}");
    key.verify(msg.as_bytes(), &sig).is_ok().then_some(account_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    const M: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon art";

    #[test]
    fn the_same_phrase_opens_the_same_account_as_the_first_server() {
        // Reference value from the first tokumai client (and its TypeScript predecessor).
        assert_eq!(from_mnemonic(M).unwrap().account_id, "c8e45eb9fcdb462252d41f382a88604e7c9f4ed75bae5e771a87c61b537a3e3b");
    }

    #[test]
    fn a_signature_is_good_for_its_purpose_and_nonce_only() {
        let a = from_mnemonic(M).unwrap();
        let sig = a.sign("chat", "n1");
        assert_eq!(account_owns(&a.public_key_pem, "chat", "n1", &sig).as_deref(), Some(a.account_id.as_str()));
        assert!(account_owns(&a.public_key_pem, "chat", "n2", &sig).is_none());
        assert!(account_owns(&a.public_key_pem, "plan", "n1", &sig).is_none());
        assert!(account_owns(&a.public_key_pem, "chat", "n1", "AAAA").is_none());
    }
}
