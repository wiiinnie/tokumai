// ---------------------------------------------------------------------------
// auth.rs — request-signature verification, the server-side mirror of the
// client's account.rs (src-tauri) and the TS client's account.ts.
//
// Byte-compatibility is the whole contract here: ids are sha256 over the exact
// SPKI-PEM string Node produces, and signatures cover exact `:`-joined strings.
// Account requests sign  `accountId:purpose:nonce`; chat requests sign
// `sessionId:counter:sha256hex(canonicalBody)`. Change either side and every
// client is silently locked out.
// ---------------------------------------------------------------------------

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

/// ASN.1 SPKI prefix for an Ed25519 public key; the 32 key bytes follow.
const SPKI_ED25519_PREFIX: [u8; 12] =
    [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];

pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// sha256 of the trimmed PEM, hex — the public "name" of an account or session.
/// Identical to the client's derivation, so ids match without a registry.
pub fn id_for(pem: &str) -> String {
    hex::encode(sha256(&[pem.trim().as_bytes()]))
}

/// Parse an SPKI-PEM Ed25519 public key (the only key format clients send).
fn verifying_key(pem: &str) -> Option<VerifyingKey> {
    let body: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<Vec<_>>()
        .join("");
    let der = B64.decode(body.trim()).ok()?;
    let raw = der.strip_prefix(&SPKI_ED25519_PREFIX[..])?;
    let bytes: [u8; 32] = raw.try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

fn verify(pem: &str, message: &str, sig_b64: &str) -> bool {
    let Some(key) = verifying_key(pem) else { return false };
    let Ok(sig_bytes) = B64.decode(sig_b64) else { return false };
    let Ok(sig) = Signature::from_slice(&sig_bytes) else { return false };
    key.verify(message.as_bytes(), &sig).is_ok()
}

/// Does this request really come from the holder of that account key?
/// Verifies the signature over `accountId:purpose:nonce` and returns the
/// accountId. Nonce replay protection is the CALLER's job (burn after this
/// returns Some, never before — a failed signature must not consume a nonce).
pub fn account_owns(public_key_pem: &str, purpose: &str, nonce: &str, sig: &str) -> Option<String> {
    let account_id = id_for(public_key_pem);
    let msg = format!("{account_id}:{purpose}:{nonce}");
    verify(public_key_pem, &msg, sig).then_some(account_id)
}

/// Verify a chat spend authorisation: the session key signed
/// `sessionId:counter:sha256hex(body)`, and the key must actually BE that
/// session (`id_for(pem) == session_id`) — which makes the check stateless.
pub fn session_authorises(
    public_key_pem: &str,
    session_id: &str,
    counter: u64,
    body: &str,
    sig: &str,
) -> bool {
    if id_for(public_key_pem) != session_id {
        return false;
    }
    let body_hash = hex::encode(sha256(&[body.as_bytes()]));
    let msg = format!("{session_id}:{counter}:{body_hash}");
    verify(public_key_pem, &msg, sig)
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn keypair() -> (SigningKey, String) {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let mut der = Vec::new();
        der.extend_from_slice(&SPKI_ED25519_PREFIX);
        der.extend_from_slice(&sk.verifying_key().to_bytes());
        let pem = format!(
            "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
            B64.encode(der)
        );
        (sk, pem)
    }

    #[test]
    fn account_signature_round_trips() {
        let (sk, pem) = keypair();
        let account_id = id_for(&pem);
        let msg = format!("{account_id}:invoice:5:nonce123");
        let sig = B64.encode(sk.sign(msg.as_bytes()).to_bytes());
        assert_eq!(account_owns(&pem, "invoice:5", "nonce123", &sig), Some(account_id));
        // wrong purpose, wrong nonce, wrong sig → all refused
        assert!(account_owns(&pem, "invoice:50", "nonce123", &sig).is_none());
        assert!(account_owns(&pem, "invoice:5", "other", &sig).is_none());
        assert!(account_owns(&pem, "invoice:5", "nonce123", "AAAA").is_none());
    }

    #[test]
    fn session_signature_binds_id_counter_and_body() {
        let (sk, pem) = keypair();
        let sid = id_for(&pem);
        let body = r#"{"model":"m","messages":[],"maxTokens":null}"#;
        let body_hash = hex::encode(sha256(&[body.as_bytes()]));
        let sig = B64.encode(sk.sign(format!("{sid}:3:{body_hash}").as_bytes()).to_bytes());
        assert!(session_authorises(&pem, &sid, 3, body, &sig));
        assert!(!session_authorises(&pem, &sid, 4, body, &sig)); // other counter
        assert!(!session_authorises(&pem, &sid, 3, "{}", &sig)); // other body
        assert!(!session_authorises(&pem, "someone-else", 3, body, &sig)); // key ≠ session
    }
}
