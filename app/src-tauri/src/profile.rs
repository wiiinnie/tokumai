//! What the app remembers between starts, in `<data dir>/profile.json`, encrypted under a
//! keychain key (`keystore`). Small on purpose: there is no money on the device any more —
//! the balance is on the account, in the enclave — so the phrase is the only secret here.

use crate::keystore::{self, EncEnvelope};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const KEYCHAIN_ACCOUNT: &str = "profile-encryption-key";
const FILE: &str = "profile.json";

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct Profile {
    /// The recovery phrase. The account, and so the balance, is derived from it.
    pub mnemonic: Option<String>,
    /// The three-word check passed (or the phrase was typed in, which is the same proof).
    pub phrase_verified: bool,
    /// An entry gateway the user picked; `None` = a random allowed one on each connect.
    pub entry_gateway: Option<String>,
    /// A card checkout that was opened and not yet seen paid.
    pub pending_plan_session: Option<String>,
    /// A front door of the enclave the user picked (its Nym address); `None` = whichever
    /// answers, starting with the first. All doors lead to the same enclave; the choice is
    /// about where our side stands, not about what the person is protected by.
    pub enclave_door: Option<String>,
    /// The mixnet speed/anonymity trade-off from the settings: [cover, mix, send] ms and
    /// whether cover traffic runs while idle. `None` = Nym's defaults.
    pub traffic: Option<(u64, u64, u64, bool)>,
    /// Blind notes minted and not yet redeemed — the wallet (see `notes` in lib.rs). A
    /// note is a bearer secret: whoever holds it can redeem it, which is why the profile
    /// is kept as the phrase is kept.
    pub notes: Vec<WalletNote>,
    /// Payments and months already turned into notes and redeemed, as "rail:epoch", so
    /// the launch sync does not ask for them again.
    pub notes_done: Vec<String>,
}

/// One paid month, signed blind by the enclave, waiting to be redeemed.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct WalletNote {
    /// The payment it came from ("iap:<original transaction>"), for the done list only.
    pub rail: String,
    pub epoch: u16,
    pub tier: u8,
    /// The note's 36 bytes and its signature, base64.
    pub note: String,
    pub sig: String,
    pub minted_ms: u64,
    /// When the app intends to redeem it: at once when the account has nothing to chat
    /// on, else a random moment inside the grace, so a renewal hides among its month's.
    pub redeem_after_ms: u64,
}

fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

fn key() -> Result<Option<[u8; 32]>, String> {
    if keystore::use_keychain() {
        keystore::keychain_key(KEYCHAIN_ACCOUNT, "profile").map(Some)
    } else {
        Ok(None)
    }
}

/// The profile, or an empty one on a first start. A file that exists but cannot be read is
/// moved aside, never overwritten: it holds the phrase.
pub fn load(dir: &Path) -> Profile {
    let p = path(dir);
    let Ok(raw) = std::fs::read_to_string(&p) else { return Profile::default() };
    match read(&raw) {
        Ok(profile) => profile,
        Err(e) => {
            let stamp = tokumai_proto::now_ms();
            let aside = dir.join(format!("profile.unreadable.{stamp}.json"));
            let _ = std::fs::rename(&p, &aside);
            log::error!("[profile] {} could not be read ({e}); kept as {}", p.display(), aside.display());
            Profile::default()
        }
    }
}

fn read(raw: &str) -> Result<Profile, String> {
    let v: serde_json::Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    if EncEnvelope::looks_like(&v) {
        let env: EncEnvelope = serde_json::from_value(v).map_err(|e| e.to_string())?;
        let key = key()?.ok_or("an encrypted profile, and no keychain to open it")?;
        serde_json::from_str(&keystore::decrypt(&key, &env)?).map_err(|e| e.to_string())
    } else {
        serde_json::from_value(v).map_err(|e| e.to_string())
    }
}

pub fn save(dir: &Path, profile: &Profile) -> Result<(), String> {
    let plain = serde_json::to_string(profile).map_err(|e| e.to_string())?;
    let text = match key()? {
        Some(k) => keystore::encrypt(&k, &plain)?,
        None => plain,
    };
    keystore::write_atomic(&path(dir), &text)
}
