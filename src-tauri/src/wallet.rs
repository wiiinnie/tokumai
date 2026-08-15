// ---------------------------------------------------------------------------
// wallet.rs — the device-local state: the recovery phrase, which derived session
// is active, and any held (withdrawn-but-not-yet-redeemed) ecash. Mirrors the
// dev backend's DevWallet, persisted as one JSON file in the app data dir.
//
// This is BEARER state: whoever holds the phrase or the ecash controls the
// money. On mobile this moves behind the OS keystore; on desktop it is a file
// in the per-app data directory.
// ---------------------------------------------------------------------------

use crate::ecash::Proof;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Default, Serialize, Deserialize)]
pub struct Wallet {
    #[serde(default)]
    pub mnemonic: Option<String>,
    /// Nym address of the scrai-server this wallet talks to.
    #[serde(default)]
    pub server: Option<String>,
    /// User-chosen entry gateway identity (base58). None = let the SDK pick one.
    #[serde(default)]
    pub entry_gateway: Option<String>,
    #[serde(default)]
    pub session_index: u32,
    /// Held ecash, grouped into packets (one packet per purchase tier).
    #[serde(default)]
    pub ecash: Vec<Vec<Proof>>,
    /// Pinned issuer keyset id — a change is a tagging red flag.
    #[serde(default)]
    pub issuer_keyset_id: Option<String>,
}

impl Wallet {
    pub fn held_total(&self) -> u64 {
        self.ecash.iter().flatten().map(|p| p.amount).sum()
    }
}

pub fn wallet_path(data_dir: &Path) -> PathBuf {
    data_dir.join("wallet.json")
}

pub fn load(data_dir: &Path) -> Wallet {
    let path = wallet_path(data_dir);
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Wallet::default(),
    }
}

pub fn save(data_dir: &Path, w: &Wallet) -> Result<(), String> {
    std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    let s = serde_json::to_string_pretty(w).map_err(|e| e.to_string())?;
    std::fs::write(wallet_path(data_dir), s).map_err(|e| e.to_string())
}
