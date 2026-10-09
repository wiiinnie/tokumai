//! Encryption at rest for what the app keeps on disk (the profile with the recovery phrase,
//! the chat vault): AES-256-GCM under a random 32-byte key that lives only in the OS
//! keychain, so a file copied off the disk or out of a backup is worthless without it.
//!
//! Where the key lives, by platform (audit M12/M13, 2026-10-08):
//! - macOS, Windows, Linux: the OS keychain / credential store, through `keyring`.
//! - iOS: the keychain, as an item that is **this device's only** — it goes into no iCloud
//!   or Finder backup and migrates to no other phone. (Through `keyring` it was the
//!   default class, backed up: an iCloud backup held the key and the files both.) The
//!   data directory is excluded from backups as well (`ios_native::exclude_from_backup`).
//! - Android: the Android Keystore wraps it — the hardware-backed key never leaves the
//!   keystore, and what is on disk is the 32 bytes under it (`Keystore.kt`). Before, the
//!   files lay in the clear in the app's private storage.
//!
//! The key is lost with the keychain (an OS reinstall without keychain migration). The
//! phrase then has to be typed in again — which is why the app insists it is written down.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The release app's service name (the first app used the same, so its entries are found
/// again). A debug build never reaches the keychain (see `dev_key`): a development binary
/// must not read — or make the OS ask the person for — the keys of the app they use.
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

/// A debug build keeps its keys in plain files under the repo's `dev-data/app-keys/`
/// (gitignored): an unsigned binary that changes with every build would otherwise make
/// macOS ask for the login password again after each one.
fn dev_key(account: &str, what: &str) -> Result<[u8; 32], String> {
    use rand::RngCore;
    let dir = std::env::var_os("TOKUMAI_DEV_DATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../dev-data")))
        .join("app-keys");
    let path = dir.join(format!("{account}.key"));
    if let Ok(b) = std::fs::read(&path) {
        return b.try_into().map_err(|_| format!("the development {what} key has the wrong length"));
    }
    let mut key = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut key);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(&path, key).map_err(|e| e.to_string())?;
    Ok(key)
}

/// The app's data directory, told once at start: where a phone keeps what goes with its
/// key (the wrapped key on Android, the migration marks on iOS).
static DATA_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

pub(crate) fn set_data_dir(dir: std::path::PathBuf) {
    let _ = DATA_DIR.set(dir);
}

#[cfg_attr(not(any(target_os = "ios", target_os = "android")), allow(dead_code))]
fn data_dir() -> Result<&'static Path, String> {
    DATA_DIR.get().map(|p| p.as_path()).ok_or_else(|| "the data directory is not known yet".to_string())
}

/// Fetch or create the key stored under `account` in the OS keychain.
pub(crate) fn keychain_key(account: &str, what: &str) -> Result<[u8; 32], String> {
    use rand::RngCore;
    // Only where the problem it solves exists. On a desktop an unsigned development binary
    // changes identity with every build, and the keychain asks for the login password after
    // each one. A phone build is signed with a stable application identifier and asks
    // nothing — and it has no `dev-data` to fall back to at all, that path being on the
    // developing Mac. (2026-09-24, from the device: "restore failed: operation not
    // permitted" was this function trying to create a folder that cannot exist on a phone.)
    if cfg!(debug_assertions) && !cfg!(any(target_os = "ios", target_os = "android")) {
        return dev_key(account, what);
    }
    #[cfg(target_os = "ios")]
    {
        return ios::device_only_key(account, what);
    }
    #[cfg(target_os = "android")]
    {
        return android::wrapped_key(account, what);
    }
    #[allow(unreachable_code)]
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

/// Whether files are encrypted under a keychain key: everywhere, since 2026-10-08 (Android
/// through the Keystore, see the module note). A file written before that, in the clear,
/// is still read, and written back sealed the next time it is saved.
pub(crate) fn use_keychain() -> bool {
    true
}

/// iOS: the keychain item is this device's only — `kSecAttrAccessibleWhenUnlockedThisDeviceOnly`
/// — so no backup carries it and no other phone receives it. `keyring` offers no way to
/// say so; this goes to the Security framework directly, under the same service and
/// account, so an item made before is found, read, and made over as this device's only.
#[cfg(target_os = "ios")]
mod ios {
    use super::{data_dir, KEYCHAIN_SERVICE};
    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;
    use security_framework::passwords::{delete_generic_password, get_generic_password, set_generic_password_options, PasswordOptions};
    use security_framework_sys::access_control::kSecAttrAccessibleWhenUnlockedThisDeviceOnly;

    // Not in security-framework-sys: the attribute key itself.
    #[link(name = "Security", kind = "framework")]
    extern "C" {
        static kSecAttrAccessible: core_foundation::string::CFStringRef;
    }

    fn device_only(account: &str) -> PasswordOptions {
        let mut options = PasswordOptions::new_generic_password(KEYCHAIN_SERVICE, account);
        #[allow(deprecated)]
        options.query.push((
            unsafe { CFString::wrap_under_get_rule(kSecAttrAccessible) },
            unsafe { CFString::wrap_under_get_rule(kSecAttrAccessibleWhenUnlockedThisDeviceOnly) }.into_CFType(),
        ));
        options
    }

    /// A mark beside the data that this account's item has been made over; the keychain
    /// does not say which class an item has, and making it over every start would be a
    /// delete-and-add of the one secret for nothing.
    fn mark(account: &str) -> Result<std::path::PathBuf, String> {
        Ok(data_dir()?.join(format!(".keychain-device-only.{account}")))
    }

    pub(super) fn device_only_key(account: &str, what: &str) -> Result<[u8; 32], String> {
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        use rand::RngCore;
        let made_over = mark(account).map(|m| m.exists()).unwrap_or(false);
        match get_generic_password(KEYCHAIN_SERVICE, account) {
            Ok(b64) => {
                let bytes = B64.decode(String::from_utf8_lossy(&b64).trim()).map_err(|e| e.to_string())?;
                let key: [u8; 32] = bytes.try_into().map_err(|_| format!("the {what} key in the keychain has the wrong length"))?;
                if !made_over {
                    // Made under the default class, by keyring: delete and add it again as
                    // this device's only. The bytes are in hand, so a failure in between
                    // puts them back under the old class rather than losing them.
                    let _ = delete_generic_password(KEYCHAIN_SERVICE, account);
                    if let Err(e) = set_generic_password_options(B64.encode(key).as_bytes(), device_only(account)) {
                        let _ = set_generic_password_options(B64.encode(key).as_bytes(), PasswordOptions::new_generic_password(KEYCHAIN_SERVICE, account));
                        return Err(format!("the {what} key could not be made this device's only: {e}"));
                    }
                    if let Ok(m) = mark(account) {
                        let _ = std::fs::write(m, b"1");
                    }
                    log::info!("[{what}] the {what} key is this device's only now");
                }
                Ok(key)
            }
            Err(e) if e.code() == security_framework_sys::base::errSecItemNotFound => {
                let mut key = [0u8; 32];
                rand::rngs::OsRng.fill_bytes(&mut key);
                set_generic_password_options(B64.encode(key).as_bytes(), device_only(account)).map_err(|e| e.to_string())?;
                if let Ok(m) = mark(account) {
                    let _ = std::fs::write(m, b"1");
                }
                log::info!("[{what}] generated a fresh {what} key in the keychain, this device's only");
                Ok(key)
            }
            Err(e) => Err(format!("keychain error: {e}")),
        }
    }
}

/// Android: the key is wrapped by a key in the Android Keystore (`Keystore.kt`), which
/// never leaves it; the wrapped bytes lie beside the data. Reached over JNI, with the
/// JavaVM the activity handed over at start (`set_vm`).
#[cfg(target_os = "android")]
pub(crate) mod android {
    use super::data_dir;
    use std::sync::atomic::{AtomicPtr, Ordering};

    static VM: AtomicPtr<jni::sys::JavaVM> = AtomicPtr::new(std::ptr::null_mut());
    static ACTIVITY: std::sync::OnceLock<jni::objects::Global<jni::objects::JObject<'static>>> = std::sync::OnceLock::new();

    /// The activity's JavaVM, from the one place Tauri hands it out (lib.rs, at start).
    pub(crate) fn set_vm(raw: *mut jni::sys::JavaVM) {
        VM.store(raw, Ordering::SeqCst);
    }

    /// The activity itself, as a global reference, for what needs a window (`owner`).
    pub(crate) fn set_activity(activity: jni::objects::Global<jni::objects::JObject<'static>>) {
        let _ = ACTIVITY.set(activity);
    }

    pub(crate) fn activity() -> Result<&'static jni::objects::Global<jni::objects::JObject<'static>>, String> {
        ACTIVITY.get().ok_or_else(|| "the activity is not known yet".to_string())
    }

    /// The VM, waiting a little for the start to hand it over: the first read of the
    /// profile can come before the main thread has run the closure that sets it.
    pub(crate) fn vm() -> Result<jni::JavaVM, String> {
        for _ in 0..100 {
            let raw = VM.load(Ordering::SeqCst);
            if !raw.is_null() {
                return Ok(unsafe { jni::JavaVM::from_raw(raw) });
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Err("the Android Keystore is not reachable yet (no JavaVM)".into())
    }

    /// `Keystore.wrap` / `Keystore.unwrap` in Kotlin: AES-GCM under the keystore key.
    fn through(method: &str, bytes: &[u8]) -> Result<Vec<u8>, String> {
        let vm = vm()?;
        vm.attach_current_thread(|env| -> Result<Vec<u8>, jni::errors::Error> {
            let input = env.byte_array_from_slice(bytes)?;
            let sig = jni::signature::RuntimeMethodSignature::from_str("([B)[B")?;
            let out = env
                .call_static_method(jni::strings::JNIString::from("com/tokumai/app/Keystore"), jni::strings::JNIString::from(method), jni::signature::MethodSignature::from(&sig), &[jni::objects::JValue::Object(&input)])?
                .l()?;
            let out = env.cast_local::<jni::objects::JByteArray>(out)?;
            env.convert_byte_array(&out)
        })
        .map_err(|e| format!("Android Keystore ({method}): {e}"))
    }

    pub(super) fn wrapped_key(account: &str, what: &str) -> Result<[u8; 32], String> {
        use rand::RngCore;
        let dir = data_dir()?.join("keys");
        let path = dir.join(format!("{account}.wrapped"));
        if let Ok(wrapped) = std::fs::read(&path) {
            let key = through("unwrap", &wrapped)?;
            return key.try_into().map_err(|_| format!("the {what} key is not 32 bytes once unwrapped"));
        }
        let mut key = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut key);
        let wrapped = through("wrap", &key)?;
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("wrapped.new");
        std::fs::write(&tmp, &wrapped).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
        log::info!("[{what}] generated a fresh {what} key, wrapped by the Android Keystore");
        Ok(key)
    }
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
