//! The doors' identities, kept by the enclave for itself.
//!
//! A door is a Nym client with an identity of its own; the identity IS the address the apps
//! pin, so it has to survive a restart. Until 2026-10-05 the operator made the identities
//! and sealed them into the secrets file — which meant the operator held every door's
//! private keys. Now the Nym client makes them inside the enclave, and what the host keeps
//! is this: the files of each client's store, sealed under a key derived from the data key
//! (which the operator does not hold either, `kms::generate_data_key_to_enclave`).
//!
//! The host stores the sealed bytes and cannot read them; a host that hands back an older
//! set gets older identities, which is the same addresses — no harm. A host that hands back
//! nothing gets doors under new addresses, which the apps will not find: that is loud, not
//! dangerous.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::path::Path;

const LABEL: &[u8] = b"tokumai/doors/v1";

/// `gateway → { file name → bytes }`, as it is sealed.
pub type Doors = Map<String, Value>;

fn cipher(data_key: &[u8; 32]) -> ChaCha20Poly1305 {
    let mut h = Sha256::new();
    h.update(LABEL);
    h.update(data_key);
    ChaCha20Poly1305::new_from_slice(&h.finalize()).expect("32 bytes")
}

pub fn seal(data_key: &[u8; 32], doors: &Doors) -> Result<Vec<u8>, String> {
    let plain = serde_json::to_vec(doors).map_err(|e| e.to_string())?;
    let nonce: [u8; 12] = rand::random();
    let ct = cipher(data_key)
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: &plain, aad: LABEL })
        .map_err(|_| "the doors could not be sealed".to_string())?;
    Ok([LABEL, b"\n", &nonce, &ct].concat())
}

pub fn open(data_key: &[u8; 32], sealed: &[u8]) -> Result<Doors, String> {
    let rest = sealed.strip_prefix(LABEL).and_then(|r| r.strip_prefix(b"\n")).ok_or("the sealed doors are not in a form this enclave knows")?;
    if rest.len() < 12 {
        return Err("the sealed doors are too short".into());
    }
    let (nonce, ct) = rest.split_at(12);
    let plain = cipher(data_key)
        .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad: LABEL })
        .map_err(|_| "the sealed doors do not open under this data key".to_string())?;
    serde_json::from_slice(&plain).map_err(|e| format!("the doors are unreadable once open: {e}"))
}

/// What of a client's store is its identity: the keys, and the registration with its
/// gateway (the shared key the gateway knows this client by). Not the reply store (SURBs
/// of exchanges that are over) and not the credentials database (bandwidth tickets, which
/// a client gets again) — both grow, and neither is the address.
const KEPT: [&str; 6] = ["private_identity.pem", "public_identity.pem", "private_encryption.pem", "public_encryption.pem", "ack_key.pem", "gateways_registrations.sqlite"];

/// The identity in `dir`, as the client left it: file name → base64. Empty if there is none.
pub fn read_dir(dir: &Path) -> Map<String, Value> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    let mut files = Map::new();
    for name in KEPT {
        if let Ok(bytes) = std::fs::read(dir.join(name)) {
            files.insert(name.to_string(), Value::String(B64.encode(bytes)));
        }
    }
    files
}

/// Lay an identity out as files, where the client expects them. Names are ours (sealed by
/// this enclave, or by the operator in the old form) — still, no paths.
pub fn lay_out(dir: &Path, files: &Map<String, Value>) -> std::io::Result<()> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    std::fs::create_dir_all(dir)?;
    for (name, content) in files {
        let name = name.rsplit('/').next().unwrap_or_default();
        if name.is_empty() || name.starts_with('.') {
            continue;
        }
        let Some(bytes) = content.as_str().and_then(|c| B64.decode(c).ok()) else { continue };
        std::fs::write(dir.join(name), bytes)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_doors_come_back_only_under_their_key() {
        let key = [7u8; 32];
        let mut doors = Doors::new();
        let mut files = Map::new();
        files.insert("private_identity.pem".into(), Value::String("QUJD".into()));
        doors.insert("gw1".into(), Value::Object(files));
        let sealed = seal(&key, &doors).unwrap();
        assert!(sealed.starts_with(LABEL));
        assert_eq!(open(&key, &sealed).unwrap(), doors);
        assert!(open(&[8u8; 32], &sealed).is_err());
        let mut bent = sealed.clone();
        let last = bent.len() - 1;
        bent[last] ^= 1;
        assert!(open(&key, &bent).is_err());
        assert!(open(&key, b"something else").is_err());
    }

    #[test]
    fn a_store_round_trips_through_the_files_that_matter() {
        let dir = std::env::temp_dir().join(format!("tokumai-doors-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("private_identity.pem"), b"id").unwrap();
        std::fs::write(dir.join("gateways_registrations.sqlite"), b"reg").unwrap();
        std::fs::write(dir.join("persistent_reply_store.sqlite"), b"big").unwrap();
        let files = read_dir(&dir);
        assert_eq!(files.len(), 2, "the reply store is not identity");
        let again = std::env::temp_dir().join(format!("tokumai-doors-{}-b", std::process::id()));
        let _ = std::fs::remove_dir_all(&again);
        let mut with_path = files.clone();
        with_path.insert("../escape.pem".into(), Value::String("QUJD".into()));
        lay_out(&again, &with_path).unwrap();
        assert_eq!(std::fs::read(again.join("private_identity.pem")).unwrap(), b"id");
        assert!(again.join("escape.pem").exists(), "the name is kept, the path is not");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&again);
    }
}
