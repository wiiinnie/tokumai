// ---------------------------------------------------------------------------
// keychain_ios.rs — the iOS Keychain, called directly.
//
// `keyring` was tried here first and abandoned (wallet.rs, 2026-08): on dev builds
// `set_password` succeeded and the next `get_password` failed, which silently reset the
// wallet. Whatever the cause, it also cannot express the two attributes this needs:
//
//   · the WALLET KEY must be `…ThisDeviceOnly` — it never leaves this phone, not in a
//     backup, not through iCloud Keychain. The encrypted wallet file is then worthless
//     anywhere but here.
//   · the RECOVERY PHRASE, when the user opts in, must be `Synchronizable` — carried by
//     iCloud Keychain, which is end-to-end encrypted: Apple stores it and cannot read it.
//
// Both are generic-password items under the app's service name. A synchronizable item
// cannot be ThisDeviceOnly (by definition), so the protection class follows `sync`.
// ---------------------------------------------------------------------------

use security_framework::access_control::{ProtectionMode, SecAccessControl};
use security_framework::passwords::{delete_generic_password_options, generic_password, set_generic_password_options};
use security_framework::passwords_options::PasswordOptions;
use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use security_framework_sys::access_control::kSecAttrAccessibleAfterFirstUnlock;
use core_foundation::string::CFStringRef as SysCFStringRef;

// The attribute KEY is not exported by security-framework-sys; the Security framework
// defines it, and the app already links that framework for everything else in this file.
#[link(name = "Security", kind = "framework")]
extern "C" {
    static kSecAttrAccessible: SysCFStringRef;
}

const SERVICE: &str = "com.tokumai.app";
const ERR_NOT_FOUND: i32 = -25300; // errSecItemNotFound

fn query(account: &str, sync: bool) -> PasswordOptions {
    let mut o = PasswordOptions::new_generic_password(SERVICE, account);
    // Explicit on every call: a search without it matches ONLY non-synchronizable items,
    // and a set without it creates one — so the phrase copy would never be found again.
    o.set_access_synchronized(Some(sync));
    o
}

/// The item's bytes, or `None` when there is no such item.
pub fn get(account: &str, sync: bool) -> Result<Option<Vec<u8>>, String> {
    match generic_password(query(account, sync)) {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.code() == ERR_NOT_FOUND => Ok(None),
        Err(e) => Err(format!("keychain read ({account}): {e}")),
    }
}

/// Write (replacing) the item. After first unlock, so a background reconnect can reach the
/// wallet key; ThisDeviceOnly unless the item is meant to travel.
pub fn set(account: &str, value: &[u8], sync: bool) -> Result<(), String> {
    let _ = delete(account, sync);
    let mut o = query(account, sync);
    if sync {
        // A synchronizable item may NOT carry a SecAccessControl object (those are bound to
        // this device's Secure Enclave) — SecItemAdd answers errSecParam, "one or more
        // parameters … not valid" (2026-09-11). The plain accessibility attribute is what
        // an iCloud-Keychain item takes.
        #[allow(deprecated)]
        unsafe {
            o.query.push((
                CFString::wrap_under_get_rule(kSecAttrAccessible),
                CFString::wrap_under_get_rule(kSecAttrAccessibleAfterFirstUnlock).into_CFType(),
            ));
        }
    } else {
        let ac = SecAccessControl::create_with_protection(Some(ProtectionMode::AccessibleAfterFirstUnlockThisDeviceOnly), 0)
            .map_err(|e| format!("keychain access control: {e}"))?;
        o.set_access_control(ac);
    }
    set_generic_password_options(value, o).map_err(|e| format!("keychain write ({account}): {e}"))
}

/// Remove the item. Absence is not an error — deleting what is already gone is the
/// outcome the caller wanted.
pub fn delete(account: &str, sync: bool) -> Result<(), String> {
    match delete_generic_password_options(query(account, sync)) {
        Ok(()) => Ok(()),
        Err(e) if e.code() == ERR_NOT_FOUND => Ok(()),
        Err(e) => Err(format!("keychain delete ({account}): {e}")),
    }
}
