//! Where the enclave's data key comes from. In production a KMS releases it only to code whose
//! attestation matches a published release (Nitro: a key policy on PCR0; Google: workload
//! identity on the image digest), so a copy of the database is useless to anyone else —
//! including the operator. On a developer's machine a local file stands in.
//!
//! The key does real work already: accounts are stored under a keyed hash of their id, so a
//! database without its key does not even say which accounts exist.

use std::path::PathBuf;

pub trait KeyProvider: Send + Sync {
    fn data_key(&self) -> Result<[u8; 32], String>;
}

/// Development: a random key in a file next to the database, made on first use.
pub struct FileKeyProvider {
    pub path: PathBuf,
}

impl KeyProvider for FileKeyProvider {
    fn data_key(&self) -> Result<[u8; 32], String> {
        if let Ok(bytes) = std::fs::read(&self.path) {
            return bytes.try_into().map_err(|_| format!("{} is not a 32-byte key", self.path.display()));
        }
        let key: [u8; 32] = rand::random();
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&self.path, key).map_err(|e| e.to_string())?;
        Ok(key)
    }
}

/// Tests: a fixed key.
pub struct FixedKeyProvider(pub [u8; 32]);

impl KeyProvider for FixedKeyProvider {
    fn data_key(&self) -> Result<[u8; 32], String> {
        Ok(self.0)
    }
}
