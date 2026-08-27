// ---------------------------------------------------------------------------
// proto.rs — the ledger request/response wire format + the crypto ACL.
//
// The ledger-service is reached over the MIXNET, so it has NO network-level access
// control (no IP allowlist, no firewall port). The access control is CRYPTOGRAPHIC
// instead: every request is signed by a federation MEMBER key, and the service only
// serves requests whose signer is on its allowlist. A per-request nonce makes a captured
// request un-replayable. This is strictly stronger than a network ACL in the mixnet model.
// ---------------------------------------------------------------------------

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Protocol version — bump on any wire-format change.
pub const PROTO_V: u32 = 1;

/// The infrequent, shareable operations (see `scrai_core::ledger::Ledger`). Billing is NOT
/// here — it stays local to the chat server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Credit redeemed value into a session; replies with the new balance.
    Credit { session_id: String, amount: u64 },
    /// Read (balance, counter) for a session.
    Status { session_id: String },
    /// Read the balance for a session.
    Balance { session_id: String },
}

impl Op {
    /// The canonical signing preimage. Fields are fixed-shape (op tag, a hex/numeric
    /// session id with no delimiters, a numeric amount, a hex nonce), so the `|`-joined
    /// form is unambiguous. A domain tag pins the purpose.
    fn signing_string(&self, nonce: &str) -> String {
        let (tag, sid, amount) = match self {
            Op::Credit { session_id, amount } => ("credit", session_id.as_str(), *amount),
            Op::Status { session_id } => ("status", session_id.as_str(), 0),
            Op::Balance { session_id } => ("balance", session_id.as_str(), 0),
        };
        format!("scrai-ledger|v{PROTO_V}|{tag}|{sid}|{amount}|{nonce}")
    }
}

/// A signed request as it travels over the mixnet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    #[serde(flatten)]
    pub op: Op,
    /// Random per-request nonce (hex) — replay protection.
    pub nonce: String,
    /// The signing member's public key (hex, 32 bytes).
    pub member: String,
    /// Ed25519 signature over `op.signing_string(nonce)` (hex, 64 bytes).
    pub sig: String,
}

/// The service's reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counter: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Reply {
    pub fn err(msg: impl Into<String>) -> Reply {
        Reply { ok: false, balance: None, counter: None, error: Some(msg.into()) }
    }
}

/// Sign an op with a member key → a ready-to-send `Request`.
pub fn sign(sk: &SigningKey, op: Op, nonce: &str) -> Request {
    let msg = op.signing_string(nonce);
    let sig: Signature = sk.sign(msg.as_bytes());
    Request {
        v: PROTO_V,
        op,
        nonce: nonce.to_string(),
        member: hex::encode(sk.verifying_key().to_bytes()),
        sig: hex::encode(sig.to_bytes()),
    }
}

/// Why a request was refused (kept separate so the service can log the reason without
/// leaking it to the caller).
#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    BadVersion,
    NotAMember,
    BadSignature,
    Replay,
    Malformed,
}

/// Verify a request against the member allowlist. Returns the verified `Op` on success.
/// The caller is responsible for the nonce set (so it owns replay state); pass whether the
/// nonce was already seen.
pub fn verify(req: &Request, allow: &[VerifyingKey], nonce_seen: bool) -> Result<Op, AuthError> {
    if req.v != PROTO_V {
        return Err(AuthError::BadVersion);
    }
    if nonce_seen {
        return Err(AuthError::Replay);
    }
    let member_bytes: [u8; 32] = hex::decode(&req.member)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or(AuthError::Malformed)?;
    let member_key = VerifyingKey::from_bytes(&member_bytes).map_err(|_| AuthError::Malformed)?;
    if !allow.iter().any(|k| k.to_bytes() == member_key.to_bytes()) {
        return Err(AuthError::NotAMember);
    }
    let sig_bytes: [u8; 64] = hex::decode(&req.sig)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or(AuthError::Malformed)?;
    let sig = Signature::from_bytes(&sig_bytes);
    let msg = req.op.signing_string(&req.nonce);
    member_key
        .verify(msg.as_bytes(), &sig)
        .map_err(|_| AuthError::BadSignature)?;
    Ok(req.op.clone())
}

/// Parse a hex member public key (for building an allowlist from config).
pub fn member_key_from_hex(hex_pk: &str) -> Result<VerifyingKey, String> {
    let bytes: [u8; 32] = hex::decode(hex_pk.trim())
        .map_err(|e| e.to_string())?
        .try_into()
        .map_err(|_| "member key must be 32 bytes".to_string())?;
    VerifyingKey::from_bytes(&bytes).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    fn member() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    #[test]
    fn sign_verify_round_trips() {
        let sk = member();
        let allow = vec![sk.verifying_key()];
        let req = sign(&sk, Op::Credit { session_id: "sess-1".into(), amount: 100_000 }, "nonce-abc");
        let op = verify(&req, &allow, false).unwrap();
        assert_eq!(op, Op::Credit { session_id: "sess-1".into(), amount: 100_000 });
    }

    #[test]
    fn non_member_is_rejected() {
        let signer = member();
        let other = member();
        let allow = vec![other.verifying_key()]; // signer NOT on the allowlist
        let req = sign(&signer, Op::Balance { session_id: "s".into() }, "n1");
        assert_eq!(verify(&req, &allow, false), Err(AuthError::NotAMember));
    }

    #[test]
    fn tampered_amount_breaks_the_signature() {
        let sk = member();
        let allow = vec![sk.verifying_key()];
        let mut req = sign(&sk, Op::Credit { session_id: "s".into(), amount: 100 }, "n2");
        // attacker inflates the amount after signing
        req.op = Op::Credit { session_id: "s".into(), amount: 1_000_000 };
        assert_eq!(verify(&req, &allow, false), Err(AuthError::BadSignature));
    }

    #[test]
    fn replayed_nonce_is_rejected() {
        let sk = member();
        let allow = vec![sk.verifying_key()];
        let req = sign(&sk, Op::Balance { session_id: "s".into() }, "n3");
        assert!(verify(&req, &allow, false).is_ok());
        assert_eq!(verify(&req, &allow, true), Err(AuthError::Replay)); // seen before
    }
}
