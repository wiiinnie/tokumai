// Several helpers (session signing, sign) are used once the ecash/chat commands
// land; silence dead-code noise until then.
#![allow(dead_code)]
// ---------------------------------------------------------------------------
// account.rs — the recoverable half of a user's money, ported from account.ts.
//
// This MUST stay byte-compatible with the TypeScript client: the scrai-server
// derives an accountId from the SPKI-PEM of the public key and verifies ed25519
// signatures over exact strings. So we reproduce Node's PEM serialisation and
// the same domain-separated key derivation, or the same server would reject us.
// ---------------------------------------------------------------------------

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use bip39::Mnemonic;
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};

/// ASN.1 SPKI prefix for an Ed25519 public key; the 32 key bytes follow.
const SPKI_ED25519_PREFIX: [u8; 12] =
    [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// The exact PEM Node's `export({type:'spki',format:'pem'})` produces for an
/// Ed25519 key. The DER (44 bytes) base64-encodes to a single 60-char line, so
/// there is no line wrapping to worry about.
fn spki_pem(pubkey: &[u8; 32]) -> String {
    let mut der = Vec::with_capacity(44);
    der.extend_from_slice(&SPKI_ED25519_PREFIX);
    der.extend_from_slice(pubkey);
    format!(
        "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
        B64.encode(der)
    )
}

/// sha256 of the trimmed PEM, hex — the public "name" of an account or session.
fn id_for(pem: &str) -> String {
    hex::encode(sha256(&[pem.trim().as_bytes()]))
}

pub struct Account {
    pub mnemonic: String,
    signing: SigningKey,
    pub public_key_pem: String,
    pub account_id: String,
}

/// A fresh account, as 24 words (256-bit entropy — see the TS note on 12 vs 24).
pub fn create_account() -> Account {
    let m = Mnemonic::generate_in(bip39::Language::English, 24).expect("mnemonic generation");
    from_mnemonic(&m.to_string()).expect("freshly generated mnemonic is valid")
}

/// Rebuild an account from its phrase. Same words in, same keys out.
pub fn from_mnemonic(phrase: &str) -> Result<Account, String> {
    let norm = phrase
        .trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let m = Mnemonic::parse_in_normalized(bip39::Language::English, &norm)
        .map_err(|_| "that is not a valid recovery phrase — check the words and their order".to_string())?;

    let seed = m.to_seed(""); // BIP39, empty passphrase — matches mnemonicToSeedSync
    let material = sha256(&[b"scrai/account/v1", &seed]);
    let signing = SigningKey::from_bytes(&material);
    let pem = spki_pem(&signing.verifying_key().to_bytes());
    let account_id = id_for(&pem);

    Ok(Account { mnemonic: norm, signing, public_key_pem: pem, account_id })
}


/// The account's public short name: to compare after restoring, and to receive credit
/// (the "top-up ID" — safe to share, it can only receive).
///
/// 16 hex characters, 64 bits. It was 8 (32 bits), which is plenty when the value is only
/// ever COMPARED — but the moment anyone types it somewhere for a payment to be credited
/// to, a collision means money reaching the wrong account. At 32 bits that becomes likely
/// around a hundred thousand accounts; at 64 it does not become likely at all. Lengthening
/// it now costs nothing, and the old value stays a prefix of the new one, so a fingerprint
/// somebody wrote down still recognisably belongs to the same account (2026-09-08).
pub fn fingerprint(account_id: &str) -> String {
    account_id
        .as_bytes()
        .chunks(4)
        .take(4)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join("-")
}

impl Account {
    /// Prove account ownership to the issuer (invoice/withdraw/entitlement).
    pub fn sign(&self, purpose: &str, nonce: &str) -> String {
        let msg = format!("{}:{}:{}", self.account_id, purpose, nonce);
        B64.encode(self.signing.sign(msg.as_bytes()).to_bytes())
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    // Reference values computed from the TypeScript client (src/money/account.ts)
    // for this fixed phrase. If these drift, the Rust core would derive a
    // different accountId/sessionId and the scrai-server would reject it.
    const M: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon art";

    #[test]
    fn account_id_matches_ts() {
        assert_eq!(
            from_mnemonic(M).unwrap().account_id,
            "c8e45eb9fcdb462252d41f382a88604e7c9f4ed75bae5e771a87c61b537a3e3b"
        );
    }

}
