// ---------------------------------------------------------------------------
// wallet.rs — the device-local state: the recovery phrase, which derived session
// is active, and any held (withdrawn-but-not-yet-redeemed) coconut credentials.
//
// This is BEARER state: whoever holds the phrase or the credentials controls
// the money. On mobile this moves behind the OS keystore; on desktop it is a
// file in the per-app data directory. Held credentials are NOT rebuildable from
// the phrase — and deliberately single-device (copying the file elsewhere risks
// a stale spend counter, which reads as a double-spend; see federation-params).
// ---------------------------------------------------------------------------

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
    /// Held coconut credentials, each a persisted `scrai_core::purse::Purse`
    /// (JSON). One entry per withdrawn ticketbook; spent-empty books are dropped.
    #[serde(default)]
    pub coconut_purses: Vec<String>,
    /// Legacy single-purse field — migrated into `coconut_purses` on load.
    /// (pub only so `..Default::default()` struct-update still works in lib.rs.)
    #[serde(default, skip_serializing)]
    pub coconut_purse: Option<String>,
}

pub fn wallet_path(data_dir: &Path) -> PathBuf {
    data_dir.join("wallet.json")
}

pub fn load(data_dir: &Path) -> Wallet {
    let path = wallet_path(data_dir);
    let mut w: Wallet = match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Wallet::default(),
    };
    // One-way migration: an old wallet's single purse becomes the first entry.
    if let Some(p) = w.coconut_purse.take() {
        w.coconut_purses.insert(0, p);
    }
    w
}

pub fn save(data_dir: &Path, w: &Wallet) -> Result<(), String> {
    std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    let s = serde_json::to_string_pretty(w).map_err(|e| e.to_string())?;
    std::fs::write(wallet_path(data_dir), s).map_err(|e| e.to_string())
}
