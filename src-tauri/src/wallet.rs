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

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A spend/redeem whose payment has LEFT the purse (counter advanced + persisted) but
/// whose server reply we haven't seen yet. Kept so an interrupted send (dropped reply
/// SURB, crash, timeout) is retried with the SAME `pay_info` — a benign quorum Replay —
/// instead of a fresh spend of new coins, which would silently burn them (H4). Cleared
/// only once the server has definitively processed it.
#[derive(Default, Serialize, Deserialize, Clone)]
pub struct PendingSpend {
    /// The serialized `scrai_core::coconut::Payment` to resend verbatim.
    pub payment: serde_json::Value,
    /// The 72-byte pay_info that binds this payment (same bytes on every retry).
    pub pay_info: Vec<u8>,
    pub spend_date: u32,
    pub coins: u64,
    /// "spend" or "redeem" — which flow to resume.
    pub kind: String,
    /// The session the coins credit, for a "redeem" resume.
    #[serde(default)]
    pub session_id: Option<String>,
}

/// A withdrawal whose request has LEFT the device but whose credential is not persisted
/// yet (M-cl-2). The server consumes paid entitlement the first time it sees the request,
/// so a lost reply or a crash before the purse is saved would lose a whole book. Kept so
/// the next collect re-sends the SAME request (same user key, same blinded request); the
/// server answers a known body from its issued cache without charging again. Cleared once
/// the purse is persisted or the server definitively refuses.
#[derive(Serialize, Deserialize, Clone)]
pub struct PendingWithdraw {
    pub server: String,
    /// `scrai_core::coconut::KeyPairUser` (JSON).
    pub user: serde_json::Value,
    /// `WithdrawalRequest` — the exact body the server keys its issued cache on.
    pub req: serde_json::Value,
    /// `RequestInfo` — the blinding openings needed to unblind the reply.
    pub req_info: serde_json::Value,
    pub expiration_date: u32,
    pub created_ms: u64,
}

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
    /// An in-flight spend/redeem awaiting its server reply — retried idempotently (H4).
    #[serde(default)]
    pub pending_spend: Option<PendingSpend>,
    /// An in-flight withdrawal awaiting its credential — resumed idempotently (M-cl-2).
    #[serde(default)]
    pub pending_withdraw: Option<PendingWithdraw>,
    /// Every Nym address of the CURRENT server (its multi-identity front doors, from the
    /// catalog reply's `identities`). Same server, same money — so when the one we use
    /// stops answering, the liveness check switches to another without any user action.
    #[serde(default)]
    pub server_alternates: Vec<String>,
}

pub fn wallet_path(data_dir: &Path) -> PathBuf {
    data_dir.join("wallet.json")
}

// --- H6: wallet-at-rest encryption ------------------------------------------
// The wallet JSON (mnemonic + bearer purses) is AES-256-GCM encrypted with a random
// 32-byte data key kept in the OS keychain, so the file is unreadable from disk or a
// backup/sync agent without the OS-protected key. `0600` + atomic writes (C2) stay.
//
// TRADEOFF: the data key lives ONLY in the keychain — if that is wiped (OS reinstall
// without keychain migration), the encrypted purses are unrecoverable. The mnemonic is
// separately recoverable (the user wrote it down); the held ecash was already single-
// device / not seed-rebuildable, so this stays consistent with the bearer model.

const KEYCHAIN_SERVICE: &str = "com.tokumai.app";
/// Pre-rebrand service name. An installed desktop app has its wallet and its chat vault
/// encrypted under keys stored here; adopting them on first run is what keeps the account
/// and the history readable across the rename. The old entries are left in place (a
/// downgrade to an older build still finds them). Drop this once no 0.4.x is in the wild.
const KEYCHAIN_SERVICE_LEGACY: &str = "com.scrambleai.app";
const KEYCHAIN_ACCOUNT: &str = "wallet-encryption-key";

/// The on-disk envelope: a versioned AEAD ciphertext, NOT the wallet in the clear.
#[derive(Serialize, Deserialize)]
pub(crate) struct EncEnvelope {
    v: u32,
    alg: String,
    /// base64(12-byte GCM nonce)
    nonce: String,
    /// base64(ciphertext ‖ tag)
    ct: String,
}

/// Fetch-or-create the 32-byte wallet key from the OS keychain.
fn wallet_key() -> Result<[u8; 32], String> {
    keychain_key(KEYCHAIN_ACCOUNT, "wallet")
}

/// Fetch-or-create a random 32-byte data key under `account` in the OS keychain
/// (service `com.tokumai.app`). Shared by the wallet and the chat vault — each has
/// its own entry, so wiping one never affects the other. A key written by a pre-rebrand
/// build is adopted rather than replaced: generating a fresh one would leave the existing
/// wallet and vault files undecryptable.
#[cfg(target_os = "ios")]
pub(crate) fn keychain_key(account: &str, what: &str) -> Result<[u8; 32], String> {
    use rand::RngCore;
    // ThisDeviceOnly, never synchronizable: this key is what makes the encrypted wallet
    // file worthless off this phone. There was no pre-rebrand iOS entry to adopt — iOS ran
    // plaintext until 2026-09-09 — so this is fetch-or-create and nothing else.
    if let Some(b64) = crate::keychain_ios::get(account, false)? {
        return decode_key(std::str::from_utf8(&b64).unwrap_or(""), what);
    }
    let mut key = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut key);
    crate::keychain_ios::set(account, B64.encode(key).as_bytes(), false)?;
    log::info!("[{what}] generated a fresh {what} encryption key in the iOS Keychain (this device only)");
    Ok(key)
}

#[cfg(not(target_os = "ios"))]
pub(crate) fn keychain_key(account: &str, what: &str) -> Result<[u8; 32], String> {
    use rand::RngCore;
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, account).map_err(|e| e.to_string())?;
    match entry.get_password() {
        Ok(b64) => decode_key(&b64, what),
        Err(keyring::Error::NoEntry) => {
            // Nothing under the new service name: either this is a fresh install, or an
            // existing one that predates the rebrand. Look before generating.
            if let Ok(legacy) = keyring::Entry::new(KEYCHAIN_SERVICE_LEGACY, account) {
                if let Ok(b64) = legacy.get_password() {
                    let key = decode_key(&b64, what)?;
                    entry.set_password(&b64).map_err(|e| e.to_string())?;
                    log::info!("[{what}] adopted the {what} key from the pre-rebrand keychain entry");
                    return Ok(key);
                }
            }
            let mut key = [0u8; 32];
            rand::rngs::OsRng.fill_bytes(&mut key);
            entry.set_password(&B64.encode(key)).map_err(|e| e.to_string())?;
            log::info!("[{what}] generated a fresh {what} encryption key in the OS keychain");
            Ok(key)
        }
        Err(e) => Err(format!("keychain error: {e}")),
    }
}

fn decode_key(b64: &str, what: &str) -> Result<[u8; 32], String> {
    let bytes = B64.decode(b64.trim()).map_err(|e| e.to_string())?;
    bytes.try_into().map_err(|_| format!("{what} key in keychain has the wrong length"))
}

/// Whether to encrypt the wallet with an OS-keychain key (H6).
///
/// iOS ran PLAINTEXT until 2026-09-09, on the argument that the container is encrypted at
/// rest by Data Protection anyway. What that argument missed: the container is in the
/// iCloud backup by default, and that backup Apple can read. So the phrase and the bearer
/// coins were leaving the phone in the clear. Two fixes together: the container is now
/// excluded from backup (lib.rs), and the wallet is encrypted under a Keychain key marked
/// ThisDeviceOnly (keychain_ios.rs) — the file is worthless anywhere but here. A legacy
/// plaintext wallet is read once and re-saved encrypted by `load`/`save` as before.
///
/// Android stays plaintext-in-sandbox: `keyring` has no backend there, and Auto Backup is
/// switched off in the manifest instead, which closes the same door.
#[cfg(target_os = "android")]
pub(crate) fn use_keychain() -> bool {
    false
}
#[cfg(not(target_os = "android"))]
pub(crate) fn use_keychain() -> bool {
    true
}

// ---- the opt-in phrase copy (iOS) ------------------------------------------------------
//
// Not the wallet key and not the wallet: the RECOVERY PHRASE alone, as a Synchronizable
// Keychain item, so iCloud Keychain carries it to the user's next phone. End to end —
// Apple stores it and cannot read it; what Apple learns is that an entry for this app
// exists under this Apple ID. Off by default, asked once after the first top-up (the
// moment there is something worth protecting), and the phrase never goes anywhere else.

#[cfg(target_os = "ios")]
const KEYCHAIN_PHRASE: &str = "recovery-phrase";

/// The synced phrase, if the user opted in on this or another of their devices.
#[cfg(target_os = "ios")]
pub(crate) fn synced_phrase() -> Result<Option<String>, String> {
    Ok(crate::keychain_ios::get(KEYCHAIN_PHRASE, true)?
        .and_then(|b| String::from_utf8(b).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty()))
}

#[cfg(target_os = "ios")]
pub(crate) fn set_synced_phrase(mnemonic: Option<&str>) -> Result<(), String> {
    match mnemonic {
        Some(m) => crate::keychain_ios::set(KEYCHAIN_PHRASE, m.trim().as_bytes(), true),
        None => crate::keychain_ios::delete(KEYCHAIN_PHRASE, true),
    }
}

/// Write `contents` to the wallet file atomically: temp + fsync + rename over the target,
/// 0600. A crash mid-write leaves the PREVIOUS wallet intact (rename is atomic) instead of a
/// truncated file that `load()` would read as empty (C2). Shared by the encrypted and the
/// plaintext (iOS) save paths.
fn write_atomic(data_dir: &Path, contents: &str) -> Result<(), String> {
    use std::io::Write;
    std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    let final_path = wallet_path(data_dir);
    let tmp_path = data_dir.join("wallet.json.tmp");
    {
        let mut f = std::fs::File::create(&tmp_path).map_err(|e| e.to_string())?;
        f.write_all(contents.as_bytes()).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp_path, &final_path).map_err(|e| e.to_string())
}

pub(crate) fn encrypt(key: &[u8; 32], plaintext: &str) -> Result<String, String> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    use rand::RngCore;
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| e.to_string())?;
    let mut nb = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nb);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nb), plaintext.as_bytes())
        .map_err(|_| "wallet encryption failed".to_string())?;
    serde_json::to_string(&EncEnvelope {
        v: 1,
        alg: "aes-256-gcm".into(),
        nonce: B64.encode(nb),
        ct: B64.encode(ct),
    })
    .map_err(|e| e.to_string())
}

pub(crate) fn decrypt(key: &[u8; 32], env: &EncEnvelope) -> Result<String, String> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| e.to_string())?;
    let nb = B64.decode(env.nonce.trim()).map_err(|e| e.to_string())?;
    let ct = B64.decode(env.ct.trim()).map_err(|e| e.to_string())?;
    let pt = cipher
        .decrypt(Nonce::from_slice(&nb), ct.as_slice())
        .map_err(|_| "wallet decryption failed (wrong key or tampered file)".to_string())?;
    String::from_utf8(pt).map_err(|e| e.to_string())
}

pub fn load(data_dir: &Path) -> Wallet {
    // The key is needed only for an ENCRYPTED file; fetch it best-effort so a legacy
    // plaintext wallet still loads if the keychain is momentarily unavailable. On iOS we
    // never use the keychain (plaintext-in-sandbox), so a stale pre-fix encrypted file just
    // reads as empty and is overwritten with plaintext on the next save (self-healing).
    let key = if use_keychain() { wallet_key().ok() } else { None };
    load_with_key(data_dir, key.as_ref())
}

fn load_with_key(data_dir: &Path, key: Option<&[u8; 32]>) -> Wallet {
    let path = wallet_path(data_dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Wallet::default(), // no file yet = legitimate first run
    };

    let mut legacy_plaintext = false;
    let mut w: Wallet = match serde_json::from_str::<serde_json::Value>(&raw) {
        // Encrypted envelope (has an "alg" field) → decrypt.
        Ok(v) if v.get("alg").and_then(|a| a.as_str()) == Some("aes-256-gcm") => {
            let env: EncEnvelope = match serde_json::from_value(v) {
                Ok(e) => e,
                Err(e) => return backup_and_default(&path, data_dir, &format!("bad envelope: {e}")),
            };
            let Some(key) = key else {
                // Keychain down: we can't read it, but we must NOT discard/overwrite it.
                // save() also needs the key and will fail, so the file stays intact.
                log::error!("[wallet] keychain unavailable — encrypted wallet left untouched on disk");
                return Wallet::default();
            };
            match decrypt(key, &env).and_then(|pt| serde_json::from_str::<Wallet>(&pt).map_err(|e| e.to_string())) {
                Ok(w) => w,
                Err(e) => return backup_and_default(&path, data_dir, &e),
            }
        }
        // Legacy plaintext wallet (pre-H6) → read it, then migrate to encrypted below.
        Ok(_) => match serde_json::from_str::<Wallet>(&raw) {
            Ok(w) => {
                legacy_plaintext = true;
                w
            }
            Err(e) => return backup_and_default(&path, data_dir, &e.to_string()),
        },
        Err(e) => return backup_and_default(&path, data_dir, &e.to_string()),
    };

    // One-way migration: an old wallet's single purse becomes the first entry.
    if let Some(p) = w.coconut_purse.take() {
        w.coconut_purses.insert(0, p);
    }
    // Migrate a legacy plaintext file to encrypted-at-rest on first load (best effort —
    // if the keychain is down, it stays plaintext and migrates on a later save).
    if legacy_plaintext {
        if let Some(key) = key {
            if let Err(e) = save_with_key(data_dir, &w, key) {
                log::error!("[wallet] could not migrate the plaintext wallet to encrypted: {e}");
            } else {
                log::info!("[wallet] migrated a legacy plaintext wallet to encrypted-at-rest");
            }
        }
    }
    w
}

/// The file EXISTS but can't be read (undecryptable / unparseable). Never return a
/// silent empty wallet that the next save() would overwrite and destroy — preserve the
/// bytes in a timestamped backup and log loudly (C2).
fn backup_and_default(path: &Path, data_dir: &Path, why: &str) -> Wallet {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let backup = data_dir.join(format!("wallet.corrupt.{stamp}.json"));
    let _ = std::fs::rename(path, &backup);
    log::error!(
        "[wallet] {} is unreadable ({why}); backed it up to {} and did NOT overwrite. \
         If it held credit, recover from that backup.",
        path.display(),
        backup.display()
    );
    Wallet::default()
}

pub fn save(data_dir: &Path, w: &Wallet) -> Result<(), String> {
    if use_keychain() {
        // Desktop: encrypt with the keychain key. A keychain failure fails the save (rather
        // than writing an unencrypted or empty wallet), protecting the on-disk wallet.
        let key = wallet_key()?;
        save_with_key(data_dir, w, &key)
    } else {
        // iOS: plaintext inside the OS-protected, app-private container (see use_keychain).
        let plaintext = serde_json::to_string_pretty(w).map_err(|e| e.to_string())?;
        write_atomic(data_dir, &plaintext)
    }
}

fn save_with_key(data_dir: &Path, w: &Wallet, key: &[u8; 32]) -> Result<(), String> {
    let plaintext = serde_json::to_string_pretty(w).map_err(|e| e.to_string())?;
    let envelope = encrypt(key, &plaintext)?;
    write_atomic(data_dir, &envelope)
}

// ---------------------------------------------------------------------------
// Regression tests for C2 + H4 + H6 (docs/security/audit-2026-08-20.md). These use the
// `*_with_key` internals with a fixed key so they never touch the real OS keychain.
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const KEY: [u8; 32] = [42u8; 32];

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("scrai-wtest-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn round_trips_and_keeps_bearer_coins() {
        let dir = scratch("rt");
        let w = Wallet {
            mnemonic: Some("abandon abandon art".into()),
            coconut_purses: vec![r#"{"purse":"one"}"#.into()],
            ..Default::default()
        };
        save_with_key(&dir, &w, &KEY).unwrap();
        let got = load_with_key(&dir, Some(&KEY));
        assert_eq!(got.mnemonic.as_deref(), Some("abandon abandon art"));
        assert_eq!(got.coconut_purses, vec![r#"{"purse":"one"}"#.to_string()]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wallet_file_is_encrypted_at_rest() {
        // H6: the mnemonic and bearer purses must NOT be readable from the file.
        let dir = scratch("enc");
        let w = Wallet {
            mnemonic: Some("correct horse battery staple".into()),
            coconut_purses: vec![r#"{"purse":"bearer-money"}"#.into()],
            ..Default::default()
        };
        save_with_key(&dir, &w, &KEY).unwrap();
        let ondisk = fs::read_to_string(wallet_path(&dir)).unwrap();
        assert!(ondisk.contains("aes-256-gcm"), "on-disk file must be the AEAD envelope");
        assert!(!ondisk.contains("correct horse"), "mnemonic must not appear in cleartext");
        assert!(!ondisk.contains("bearer-money"), "purse must not appear in cleartext");
        // …and it decrypts back to the same wallet.
        assert_eq!(load_with_key(&dir, Some(&KEY)).mnemonic.as_deref(), Some("correct horse battery staple"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn encrypt_decrypt_round_trips_and_rejects_a_wrong_key() {
        let pt = r#"{"mnemonic":"x","coconut_purses":["money"]}"#;
        let env: EncEnvelope = serde_json::from_str(&encrypt(&KEY, pt).unwrap()).unwrap();
        assert_eq!(decrypt(&KEY, &env).unwrap(), pt);
        assert!(decrypt(&[9u8; 32], &env).is_err(), "GCM auth must reject a wrong key");
    }

    #[test]
    fn wrong_key_is_backed_up_not_silently_returned_empty() {
        let dir = scratch("wrongkey");
        save_with_key(&dir, &Wallet { mnemonic: Some("s".into()), ..Default::default() }, &KEY).unwrap();
        // A different key can't decrypt → must NOT silently return the wallet, and must
        // preserve the undecryptable file rather than overwrite it.
        let got = load_with_key(&dir, Some(&[7u8; 32]));
        assert!(got.mnemonic.is_none(), "a wrong key must never yield the wallet");
        let backups = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("wallet.corrupt."))
            .count();
        assert_eq!(backups, 1, "the undecryptable wallet must be preserved as a backup");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn legacy_plaintext_wallet_migrates_to_encrypted() {
        // A pre-H6 plaintext wallet.json must still load AND be rewritten encrypted.
        let dir = scratch("migrate");
        let plain = serde_json::to_string_pretty(&Wallet {
            mnemonic: Some("legacy words".into()),
            coconut_purses: vec![r#"{"purse":"legacy-money"}"#.into()],
            ..Default::default()
        })
        .unwrap();
        fs::write(wallet_path(&dir), &plain).unwrap();

        let got = load_with_key(&dir, Some(&KEY));
        assert_eq!(got.mnemonic.as_deref(), Some("legacy words"));
        let ondisk = fs::read_to_string(wallet_path(&dir)).unwrap();
        assert!(ondisk.contains("aes-256-gcm"), "must be encrypted after migration");
        assert!(!ondisk.contains("legacy words"), "plaintext must be gone after migration");
        assert_eq!(
            load_with_key(&dir, Some(&KEY)).coconut_purses,
            vec![r#"{"purse":"legacy-money"}"#.to_string()]
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_wallet_is_backed_up_never_silently_discarded() {
        let dir = scratch("corrupt");
        let w = Wallet {
            mnemonic: Some("secret phrase".into()),
            coconut_purses: vec![r#"{"purse":"bearer-money"}"#.into()],
            ..Default::default()
        };
        save_with_key(&dir, &w, &KEY).unwrap();
        // Simulate on-disk corruption: keep only the first half of the bytes.
        let full = fs::read_to_string(wallet_path(&dir)).unwrap();
        let corrupt = full[..full.len() / 2].to_string();
        fs::write(wallet_path(&dir), &corrupt).unwrap();

        let _ = load_with_key(&dir, Some(&KEY));
        let backups: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("wallet.corrupt."))
            .collect();
        assert_eq!(backups.len(), 1, "corrupt wallet must be preserved in a backup, not discarded");
        let saved = fs::read_to_string(backups[0].path()).unwrap();
        assert_eq!(saved, corrupt, "backup must be the exact on-disk bytes, for manual recovery");
        // a subsequent save writes a fresh wallet WITHOUT touching the backup
        save_with_key(&dir, &Wallet::default(), &KEY).unwrap();
        assert!(backups[0].path().exists(), "backup must survive the next save()");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pending_spend_survives_a_restart() {
        // H4: an in-flight spend's retry record must persist, or a dropped reply after
        // a restart would re-spend fresh coins instead of replaying the same payment.
        let dir = scratch("pending");
        let w = Wallet {
            mnemonic: Some("x".into()),
            pending_spend: Some(PendingSpend {
                payment: serde_json::json!({ "ss": [1, 2, 3] }),
                pay_info: vec![7u8; 72],
                spend_date: 123,
                coins: 5,
                kind: "redeem".into(),
                session_id: Some("sid".into()),
            }),
            ..Default::default()
        };
        save_with_key(&dir, &w, &KEY).unwrap();
        let p = load_with_key(&dir, Some(&KEY)).pending_spend.expect("pending must survive a restart");
        assert_eq!(p.pay_info, vec![7u8; 72], "same pay_info must be kept for the replay");
        assert_eq!(p.coins, 5);
        assert_eq!(p.kind, "redeem");
        assert_eq!(p.session_id.as_deref(), Some("sid"));
        fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn saved_wallet_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("perms");
        save_with_key(&dir, &Wallet { mnemonic: Some("x".into()), ..Default::default() }, &KEY).unwrap();
        let mode = fs::metadata(wallet_path(&dir)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "wallet holds bearer money + the seed — must be 0600, was {mode:o}");
        fs::remove_dir_all(&dir).ok();
    }
}
