// ---------------------------------------------------------------------------
// service.rs — the ledger-service request handler.
//
// This is what a Nym service-provider loop calls for each received message: it verifies
// the member signature + nonce, runs the op against the local hardened store, and returns
// the reply bytes. The loop itself (receiving over the mixnet, replying via the SURB) is
// a thin wrapper mirroring the chat server's main loop — it is the ONLY part that needs a
// live mixnet to exercise. This handler is pure and fully unit-tested.
// ---------------------------------------------------------------------------

use std::collections::HashSet;

use ed25519_dalek::VerifyingKey;
use scrai_core::ledger::Ledger;

use crate::proto::{self, AuthError, Op, Reply, Request};
use crate::store::LedgerStore;

/// Everything the handler needs: the store, the member allowlist, and the replay set.
pub struct Service {
    pub store: LedgerStore,
    pub allow: Vec<VerifyingKey>,
    /// Seen nonces (replay protection). Bounded in a real deployment (LRU / windowed);
    /// unbounded here is fine for the handler's contract + tests.
    seen_nonces: HashSet<String>,
}

impl Service {
    pub fn new(store: LedgerStore, allow: Vec<VerifyingKey>) -> Service {
        Service { store, allow, seen_nonces: HashSet::new() }
    }

    /// Handle one raw request envelope → raw reply bytes. Never panics; a malformed or
    /// unauthorized request gets a generic error reply (no internals leaked).
    pub async fn handle(&mut self, request: &[u8]) -> Vec<u8> {
        let reply = self.handle_reply(request).await;
        serde_json::to_vec(&reply).unwrap_or_default()
    }

    async fn handle_reply(&mut self, request: &[u8]) -> Reply {
        let req: Request = match serde_json::from_slice(request) {
            Ok(r) => r,
            Err(_) => return Reply::err("malformed request"),
        };
        let seen = self.seen_nonces.contains(&req.nonce);
        let op = match proto::verify(&req, &self.allow, seen) {
            Ok(op) => op,
            Err(e) => {
                // A generic message to the caller; the reason stays server-side.
                let msg = match e {
                    AuthError::Replay => "replayed request",
                    AuthError::NotAMember | AuthError::BadSignature => "unauthorized",
                    AuthError::BadVersion => "unsupported protocol version",
                    AuthError::Malformed => "malformed request",
                };
                return Reply::err(msg);
            }
        };
        // The signature is valid AND fresh — burn the nonce before doing the work so a
        // concurrent duplicate can't slip through (the loop is serial, but this keeps the
        // invariant explicit).
        self.seen_nonces.insert(req.nonce.clone());

        match op {
            Op::Credit { session_id, amount } => match self.store.session_credit(&session_id, amount).await {
                Ok(balance) => Reply { ok: true, balance: Some(balance), counter: None, error: None },
                Err(e) => Reply::err(format!("credit failed: {e}")),
            },
            Op::Status { session_id } => match self.store.session_status(&session_id).await {
                Ok((balance, counter)) => {
                    Reply { ok: true, balance: Some(balance), counter: Some(counter), error: None }
                }
                Err(e) => Reply::err(format!("status failed: {e}")),
            },
            Op::Balance { session_id } => match self.store.session_balance(&session_id).await {
                Ok(balance) => Reply { ok: true, balance: Some(balance), counter: None, error: None },
                Err(e) => Reply::err(format!("balance failed: {e}")),
            },
        }
    }
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::sign;
    use ed25519_dalek::SigningKey;
    use futures::executor::block_on;
    use rand::rngs::OsRng;

    fn reply_of(bytes: &[u8]) -> Reply {
        serde_json::from_slice(bytes).unwrap()
    }

    #[test]
    fn authorized_credit_then_balance() {
        let member = SigningKey::generate(&mut OsRng);
        let mut svc = Service::new(LedgerStore::in_memory().unwrap(), vec![member.verifying_key()]);

        let credit = sign(&member, Op::Credit { session_id: "s1".into(), amount: 100_000 }, "n-1");
        let r = reply_of(&block_on(svc.handle(&serde_json::to_vec(&credit).unwrap())));
        assert!(r.ok);
        assert_eq!(r.balance, Some(100_000));

        let bal = sign(&member, Op::Balance { session_id: "s1".into() }, "n-2");
        let r = reply_of(&block_on(svc.handle(&serde_json::to_vec(&bal).unwrap())));
        assert_eq!(r.balance, Some(100_000));
    }

    #[test]
    fn a_non_member_cannot_credit() {
        let member = SigningKey::generate(&mut OsRng);
        let outsider = SigningKey::generate(&mut OsRng);
        let mut svc = Service::new(LedgerStore::in_memory().unwrap(), vec![member.verifying_key()]);

        let credit = sign(&outsider, Op::Credit { session_id: "s1".into(), amount: 999_999 }, "x-1");
        let r = reply_of(&block_on(svc.handle(&serde_json::to_vec(&credit).unwrap())));
        assert!(!r.ok);
        // and nothing was credited
        let bal = sign(&member, Op::Balance { session_id: "s1".into() }, "x-2");
        let r = reply_of(&block_on(svc.handle(&serde_json::to_vec(&bal).unwrap())));
        assert_eq!(r.balance, Some(0));
    }

    #[test]
    fn a_replayed_credit_does_not_double_credit() {
        let member = SigningKey::generate(&mut OsRng);
        let mut svc = Service::new(LedgerStore::in_memory().unwrap(), vec![member.verifying_key()]);

        let credit = sign(&member, Op::Credit { session_id: "s1".into(), amount: 100_000 }, "same-nonce");
        let bytes = serde_json::to_vec(&credit).unwrap();
        let first = reply_of(&block_on(svc.handle(&bytes)));
        assert!(first.ok);
        // replay the identical signed request → rejected, balance unchanged
        let second = reply_of(&block_on(svc.handle(&bytes)));
        assert!(!second.ok);

        let bal = sign(&member, Op::Balance { session_id: "s1".into() }, "check");
        let r = reply_of(&block_on(svc.handle(&serde_json::to_vec(&bal).unwrap())));
        assert_eq!(r.balance, Some(100_000)); // NOT 200_000
    }
}
