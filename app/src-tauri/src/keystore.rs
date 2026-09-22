//! Encryption at rest for what the app keeps on disk (the profile with the recovery phrase,
//! the chat vault): AES-256-GCM under a random 32-byte key that lives only in the OS
//! keychain, so a file copied off the disk or out of a backup is worthless without it.
//!
//! The key is lost with the keychain (an OS reinstall without keychain migration). The
//! phrase then has to be typed in again — which is why the app insists it is written down.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Shared with the first app, so an installed one's keychain entries are found again.
const KEYCHAIN_SERVICE: &str = "com.tokumai.app";

/// A versioned AEAD envelope — what is on disk instead of the data.
#[derive(Serialize, Deserialize)]
pub(crate) struct EncEnvelope {
    v: u32,
    alg: String,
    /// base64(12-byte GCM nonce)
    nonce: String,
    /// base64(ciphertext ‖ tag)
    ct: String,
}

impl EncEnvelope {
    pub(crate) fn looks_like(v: &serde_json::Value) -> bool {
        v.get("alg").and_then(|a| a.as_str()) == Some("aes-256-gcm")
    }
}

/// Fetch or create the key stored under `account` in the OS keychain.
pub(crate) fn keychain_key(account: &str, what: &str) -> Result<[u8; 32], String> {
    use rand::RngCore;
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, account).map_err(|e| e.to_string())?;
    match entry.get_password() {
        Ok(b64) => {
            let bytes = B64.decode(b64.trim()).map_err(|e| e.to_string())?;
            bytes.try_into().map_err(|_| format!("the {what} key in the keychain has the wrong length"))
        }
        Err(keyring::Error::NoEntry) => {
            let mut key = [0u8; 32];
            rand::rngs::OsRng.fill_bytes(&mut key);
            entry.set_password(&B64.encode(key)).map_err(|e| e.to_string())?;
            log::info!("[{what}] generated a fresh {what} key in the OS keychain");
            Ok(key)
        }
        Err(e) => Err(format!("keychain error: {e}")),
    }
}

/// Whether files are encrypted under a keychain key. Android has no keychain backend in
/// `keyring`; there the app sandbox (with Auto Backup off) is the protection.
pub(crate) fn use_keychain() -> bool {
    !cfg!(target_os = "android")
}

pub(crate) fn encrypt(key: &[u8; 32], plaintext: &str) -> Result<String, String> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    use rand::RngCore;
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| e.to_string())?;
    let mut nb = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nb);
    let ct = cipher.encrypt(Nonce::from_slice(&nb), plaintext.as_bytes()).map_err(|_| "encryption failed".to_string())?;
    serde_json::to_string(&EncEnvelope { v: 1, alg: "aes-256-gcm".into(), nonce: B64.encode(nb), ct: B64.encode(ct) }).map_err(|e| e.to_string())
}

pub(crate) fn decrypt(key: &[u8; 32], env: &EncEnvelope) -> Result<String, String> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| e.to_string())?;
    let nb = B64.decode(env.nonce.trim()).map_err(|e| e.to_string())?;
    let ct = B64.decode(env.ct.trim()).map_err(|e| e.to_string())?;
    let pt = cipher.decrypt(Nonce::from_slice(&nb), ct.as_slice()).map_err(|_| "decryption failed (wrong key or a damaged file)".to_string())?;
    String::from_utf8(pt).map_err(|e| e.to_string())
}

/// Write `text` to `path` so that a crash leaves either the old file or the new one, never
/// half of either; readable by the owner only.
pub(crate) fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let dir = path.parent().ok_or("no parent directory")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_sealed_opens_with_the_key_and_only_with_it() {
        let env: EncEnvelope = serde_json::from_str(&encrypt(&[7; 32], "phrase").unwrap()).unwrap();
        assert_eq!(decrypt(&[7; 32], &env).unwrap(), "phrase");
        assert!(decrypt(&[8; 32], &env).is_err());
    }
}
