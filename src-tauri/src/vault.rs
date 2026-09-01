// vault.rs — the chat history store, in Rust, keyed from the OS keychain.
//
// Until 0.4.1 the webview kept sessions AES-GCM encrypted in IndexedDB — with the raw
// 32-byte key in the SAME IndexedDB (`keys` store, "device-aes-key"). Copying the profile
// folder (backup, sync client, another user on the machine) yielded ciphertext and key
// together, so the encryption bought nothing at rest. A Windows tester found it in
// `…\EBWebView\Default\IndexedDB\…\000003.log` (2026-08-30).
//
// Now every session is one file `<data_dir>/vault/<id>.json` holding an AES-256-GCM
// envelope (same format as wallet.json) under a key that lives ONLY in the OS keychain
// (macOS Keychain / Windows Credential Manager / Linux secret service), entry
// `com.scrambleai.app / vault-encryption-key`. The webview only ever sees plaintext
// sessions over IPC: the key never enters JS, neither on disk nor in memory, so an XSS
// in the webview can read what the user is looking at but cannot exfiltrate the key.
//
// iOS / Android: no keychain (see wallet::use_keychain — the keyring backend is unreliable
// on iOS dev builds and absent on Android); the files sit in the app-private container,
// which the OS encrypts at rest (Data Protection / file-based encryption). That is the
// same protection the wallet gets there. Vault files are then stored in the clear inside
// that container — honest, and no weaker than "key next to ciphertext" was.
//
// Writes are atomic (temp + fsync + rename) so a crash never truncates a session.

use crate::wallet::{decrypt, encrypt, use_keychain, EncEnvelope};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const KEYCHAIN_ACCOUNT: &str = "vault-encryption-key";
const DIR: &str = "vault";

/// What is encrypted: the session as the webview hands it over, plus its own clock.
#[derive(Serialize, Deserialize, Debug)]
struct Record {
    updated: u64,
    session: Value,
}

/// Sidebar metadata — the only thing `list` returns.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Meta {
    pub id: String,
    pub title: String,
    pub model: Option<String>,
    pub updated: u64,
    pub count: usize,
}

/// The vault key is fetched from the keychain once per process; macOS would otherwise
/// consult the keychain on every keystroke-driven save.
static KEY: OnceLock<Option<[u8; 32]>> = OnceLock::new();

fn key() -> Result<Option<[u8; 32]>, String> {
    if !use_keychain() {
        return Ok(None);
    }
    if let Some(k) = KEY.get() {
        return Ok(*k);
    }
    let k = crate::wallet::keychain_key(KEYCHAIN_ACCOUNT, "vault")?;
    Ok(*KEY.get_or_init(|| Some(k)))
}

pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join(DIR)
}

/// Session ids come from the webview: only the uuid alphabet may reach the filesystem.
fn check_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err("invalid session id".into())
    }
}

fn path(data_dir: &Path, id: &str) -> Result<PathBuf, String> {
    check_id(id)?;
    Ok(dir(data_dir).join(format!("{id}.json")))
}

fn seal(key: Option<&[u8; 32]>, rec: &Record) -> Result<String, String> {
    let plain = serde_json::to_string(rec).map_err(|e| e.to_string())?;
    match key {
        Some(k) => encrypt(k, &plain),
        None => Ok(plain),
    }
}

fn open(key: Option<&[u8; 32]>, raw: &str) -> Result<Record, String> {
    let v: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    if v.get("alg").and_then(|a| a.as_str()) == Some("aes-256-gcm") {
        let k = key.ok_or("vault file is encrypted but no keychain key is available")?;
        let env: EncEnvelope = serde_json::from_value(v).map_err(|e| e.to_string())?;
        serde_json::from_str(&decrypt(k, &env)?).map_err(|e| e.to_string())
    } else {
        serde_json::from_value(v).map_err(|e| e.to_string())
    }
}

fn write_atomic(target: &Path, contents: &str) -> Result<(), String> {
    use std::io::Write;
    let parent = target.parent().ok_or("vault path has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let tmp = target.with_extension("json.tmp");
    {
        let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
        f.write_all(contents.as_bytes()).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, target).map_err(|e| e.to_string())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn meta_of(rec: &Record) -> Option<Meta> {
    let s = &rec.session;
    let id = s.get("id")?.as_str()?.to_string();
    let title = s.get("title").and_then(|t| t.as_str()).filter(|t| !t.is_empty()).unwrap_or("untitled").to_string();
    let model = s.get("model").and_then(|m| m.as_str()).map(str::to_string);
    let count = s.get("messages").and_then(|m| m.as_array()).map(|a| a.len()).unwrap_or(0);
    Some(Meta { id, title, model, updated: rec.updated, count })
}

// ---- the store ---------------------------------------------------------------

/// Newest first. Unreadable files are skipped (never fail the whole sidebar), but logged.
pub fn list(data_dir: &Path) -> Result<Vec<Meta>, String> {
    let key = key()?;
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir(data_dir)) {
        Ok(e) => e,
        Err(_) => return Ok(out), // no vault yet
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match std::fs::read_to_string(&p).map_err(|e| e.to_string()).and_then(|raw| open(key.as_ref(), &raw)) {
            Ok(rec) => {
                if let Some(m) = meta_of(&rec) {
                    out.push(m);
                }
            }
            Err(e) => log::warn!("[vault] skipping unreadable {}: {e}", p.display()),
        }
    }
    out.sort_by(|a, b| b.updated.cmp(&a.updated));
    Ok(out)
}

pub fn load(data_dir: &Path, id: &str) -> Result<Option<Value>, String> {
    let p = path(data_dir, id)?;
    let raw = match std::fs::read_to_string(&p) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    Ok(Some(open(key()?.as_ref(), &raw)?.session))
}

/// `updated` lets the IndexedDB migration keep the original timestamps; a live save
/// passes `None` = now.
pub fn save(data_dir: &Path, session: Value, updated: Option<u64>) -> Result<Meta, String> {
    let id = session.get("id").and_then(|i| i.as_str()).ok_or("session without id")?.to_string();
    let p = path(data_dir, &id)?;
    let rec = Record { updated: updated.unwrap_or_else(now_ms), session };
    let meta = meta_of(&rec).ok_or("session without id")?;
    write_atomic(&p, &seal(key()?.as_ref(), &rec)?)?;
    Ok(meta)
}

// ---- pending payments --------------------------------------------------------
// Open invoices (incl. the testnet faucet memo) used to persist in webview
// localStorage — plaintext in WebView2/WebKit LevelDB files, where "deleted" values
// linger until compaction (Windows tester finding, 2026-09-01). They now live here:
// one encrypted file beside the chat vault, removed outright once nothing is pending.
// The ".enc" extension keeps list() (which only reads *.json) from ever showing it
// as a conversation.
const PENDING_FILE: &str = "pending.enc";

pub fn pending_load(data_dir: &Path) -> Result<Value, String> {
    let p = dir(data_dir).join(PENDING_FILE);
    let raw = match std::fs::read_to_string(&p) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Value::Array(Vec::new())),
        Err(e) => return Err(e.to_string()),
    };
    Ok(open(key()?.as_ref(), &raw)?.session)
}

pub fn pending_save(data_dir: &Path, list: Value) -> Result<(), String> {
    let p = dir(data_dir).join(PENDING_FILE);
    if list.as_array().map(|a| a.is_empty()).unwrap_or(false) {
        // nothing pending — leave no file behind at all
        return match std::fs::remove_file(&p) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        };
    }
    let rec = Record { updated: now_ms(), session: list };
    write_atomic(&p, &seal(key()?.as_ref(), &rec)?)
}

pub fn remove(data_dir: &Path, id: &str) -> Result<(), String> {
    let p = path(data_dir, id)?;
    match std::fs::remove_file(&p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("scrai-vault-test-{}-{}", std::process::id(), crate::rand_hex(4)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn session(id: &str, title: &str, n: usize) -> Value {
        let msgs: Vec<Value> = (0..n).map(|i| json!({"role": if i % 2 == 0 {"you"} else {"ai"}, "text": format!("m{i}")})).collect();
        json!({"id": id, "title": title, "model": "gemini-3.5-flash", "created": "2026-08-30T10:00:00Z", "messages": msgs})
    }

    #[test]
    fn round_trip_is_sealed_and_listed_newest_first() {
        let d = tmp();
        let key = [7u8; 32];
        let a = Record { updated: 100, session: session("aaaa-1", "first", 2) };
        let b = Record { updated: 200, session: session("bbbb-2", "", 5) };
        for r in [&a, &b] {
            let id = r.session["id"].as_str().unwrap();
            write_atomic(&path(&d, id).unwrap(), &seal(Some(&key), r).unwrap()).unwrap();
        }
        // on disk: an envelope, not the chat
        let raw = std::fs::read_to_string(path(&d, "aaaa-1").unwrap()).unwrap();
        assert!(raw.contains("aes-256-gcm") && !raw.contains("first") && !raw.contains("m0"));
        // wrong key → unreadable; right key → the session
        assert!(open(Some(&[8u8; 32]), &raw).is_err());
        let rec = open(Some(&key), &raw).unwrap();
        assert_eq!(rec.updated, 100);
        assert_eq!(rec.session, a.session);
        // metadata: title fallback + count, sorted by updated desc
        let mut metas: Vec<Meta> = [&a, &b].iter().map(|r| meta_of(r).unwrap()).collect();
        metas.sort_by(|x, y| y.updated.cmp(&x.updated));
        assert_eq!(metas[0].id, "bbbb-2");
        assert_eq!(metas[0].title, "untitled");
        assert_eq!(metas[0].count, 5);
        assert_eq!(metas[1].model.as_deref(), Some("gemini-3.5-flash"));
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn plaintext_records_open_without_a_key_and_ids_are_checked() {
        let d = tmp();
        let r = Record { updated: 1, session: session("cccc-3", "mobile", 1) };
        let raw = seal(None, &r).unwrap();
        assert!(raw.contains("mobile"));
        assert_eq!(open(None, &raw).unwrap().session, r.session);
        // an encrypted file with no key available must not pretend to be empty
        let enc = seal(Some(&[1u8; 32]), &r).unwrap();
        assert!(open(None, &enc).unwrap_err().contains("no keychain key"));
        for bad in ["", "../x", "a/b", "x\\y", "id with space", &"z".repeat(65)] {
            assert!(path(&d, bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(path(&d, "3f2a-9c_ok").is_ok());
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn remove_of_a_missing_session_is_fine() {
        let d = tmp();
        assert!(remove(&d, "never-there").is_ok());
        std::fs::remove_dir_all(d).ok();
    }
}
