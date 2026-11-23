//! The Memory Palace on this device: the copy that counts here, the queue of what still
//! has to reach the enclave, and the sync with the other devices of the same phrase.
//!
//! Local first. A change is merged into the palace and sealed to `<data_dir>/palace.bin`
//! (under the palace's own key, from the phrase) before anything else happens; from that
//! moment the memory counts as updated. The blob(s) it touched go into a queue that is
//! kept in the same file, so it survives a restart, and is drained after a random delay
//! (5–120 s), never within `SPEND_GAP` of a blind note being minted or redeemed.
//!
//! What protects the person (`tokumai_core::palace`): the enclave sees a namespace that
//! no account names, blobs under HMAC names, and ciphertext. Every palace request goes
//! over a mixnet client of its own (not the account's), in an envelope signed by a key
//! made for that one request and thrown away; inside, it is signed by the palace's key.
//!
//! The interface sees plaintext — it shows the palace, builds the index that goes with a
//! question, answers `read_memory` — but never a key.

use crate::keystore::write_atomic;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use tokumai_core::palace::{self as core, FileEntry, Line, Manifest, MemFile, PalaceKeys, Placeholder, Placeholders, SignedFields, MANIFEST_ID, PLACEHOLDERS_ID};

const FILE: &str = "palace.bin";
const CONFIG: &str = "palace-config.json";
/// How long after a blind note was minted or redeemed no palace request goes out, and
/// the delay a change waits before it is uploaded.
pub const SPEND_GAP_MS: u64 = 90_000;
const UPLOAD_DELAY_MS: (u64, u64) = (5_000, 120_000);

/// Whether this is an upload window: a session's start (the app started, or came back
/// after an hour away, or "Sync now"). What the palace learns during a session is kept
/// on the device and goes up at the NEXT one: hours or days between a conversation and
/// the upload it caused, not seconds — so the namespace's writes cannot be lined up with
/// an account's questions by their timing (2026-11-23). Closed once the queue is empty.
pub static UPLOAD_WINDOW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
/// After this many failed attempts in a row the indicator says so (and keeps trying).
const ATTEMPTS_BEFORE_ERROR: u32 = 5;
/// The longest wait between two attempts.
const BACKOFF_MAX_MS: u64 = 30 * 60_000;

/// When the account last spent or minted a blind note (`lib.rs` notes the moment).
pub static LAST_SPEND_MS: AtomicU64 = AtomicU64::new(0);

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn rand_between(lo: u64, hi: u64) -> u64 {
    lo + rand::random::<u64>() % (hi - lo + 1)
}

/// What the person chose. Nothing secret: on / off / asked yet, and whether the palace
/// stays on this device only.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// None until the person has been asked (the palace is opt-in).
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default, rename = "localOnly")]
    pub local_only: bool,
    /// "Delete entire Memory Palace" was chosen and the enclave's copy is not gone yet:
    /// tried again until it is, and nothing is synced meanwhile — or the old palace would
    /// come back from the server. Kept here so it survives a restart.
    #[serde(default, rename = "wipePending")]
    pub wipe_pending: bool,
}

pub fn config(dir: &Path) -> Config {
    std::fs::read_to_string(dir.join(CONFIG)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn set_config(dir: &Path, c: &Config) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    write_atomic(&dir.join(CONFIG), &serde_json::to_string(c).map_err(|e| e.to_string())?)
}

/// Everything kept on this device, sealed as one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Local {
    pub manifest: Manifest,
    pub files: BTreeMap<String, MemFile>,
    pub placeholders: Placeholders,
    /// The server's version of each blob (by file id) as last seen or written.
    pub remote: BTreeMap<String, u64>,
    /// Blobs (by file id) changed here and not yet uploaded, with the local revision of
    /// the change — an upload clears an entry only if nothing changed since.
    pub dirty: BTreeMap<String, u64>,
    /// Files deleted here whose blobs are still on the server.
    pub gone: BTreeSet<String>,
    pub rev: u64,
    pub due_ms: u64,
    pub attempts: u32,
    pub last_sync_ms: u64,
    pub last_error: Option<String>,
    /// The last attempt failed on the way (no route), not at the enclave.
    pub offline: bool,
}

fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

/// The palace as kept here. A file that does not open with this phrase (another account
/// was on the device) is started afresh.
pub fn load(dir: &Path, keys: &PalaceKeys) -> Local {
    let Ok(raw) = std::fs::read(path(dir)) else { return Local::default() };
    keys.open_local(&raw).ok().and_then(|p| serde_json::from_slice(&p).ok()).unwrap_or_default()
}

pub fn save(dir: &Path, keys: &PalaceKeys, l: &Local) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let sealed = keys.seal_local(&serde_json::to_vec(l).map_err(|e| e.to_string())?);
    let tmp = dir.join(format!("{FILE}.tmp"));
    std::fs::write(&tmp, &sealed).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path(dir)).map_err(|e| e.to_string())
}

pub fn forget(dir: &Path) {
    let _ = std::fs::remove_file(path(dir));
}

impl Local {
    fn touch(&mut self, file_id: &str, now: u64) {
        self.rev += 1;
        self.dirty.insert(file_id.to_string(), self.rev);
        if self.due_ms == 0 || self.attempts == 0 && self.due_ms < now {
            self.due_ms = now + rand_between(UPLOAD_DELAY_MS.0, UPLOAD_DELAY_MS.1);
        }
    }

    fn entry_mut(&mut self, file_id: &str) -> Option<&mut FileEntry> {
        self.manifest.files.iter_mut().find(|f| f.id == file_id)
    }

    fn title_of(&self, file_id: &str) -> String {
        self.manifest.files.iter().find(|f| f.id == file_id).map(|f| f.title.clone()).unwrap_or_default()
    }

    fn file_changed(&mut self, file_id: &str, now: u64) {
        if let Some(e) = self.entry_mut(file_id) {
            e.version += 1;
        }
        self.touch(file_id, now);
        self.touch(MANIFEST_ID, now);
    }

    /// The live files and lines, for the interface.
    pub fn view(&self) -> Value {
        let files: Vec<Value> = self
            .manifest
            .files
            .iter()
            .filter(|f| !f.deleted)
            .map(|f| {
                let lines: Vec<&Line> = self.files.get(&f.id).map(|m| m.lines.iter().filter(|l| !l.deleted).collect()).unwrap_or_default();
                json!({ "id": f.id, "title": f.title, "description": f.description, "neverSend": f.never_send, "version": f.version, "updated": f.updated, "lines": lines })
            })
            .collect();
        json!({ "files": files, "placeholders": self.placeholders.map })
    }

    /// The text form of everything, for an export the person asked for.
    pub fn export(&self) -> Value {
        json!({ "schema": "tokumai/palace-export@1", "exported": now_ms(), "manifest": self.manifest, "files": self.files.values().collect::<Vec<_>>(), "placeholders": self.placeholders })
    }
}

/// One change, as the interface (or the extraction) asks for it.
#[derive(Debug, Clone, Deserialize)]
pub struct Op {
    pub op: String,
    #[serde(default, rename = "fileId")]
    pub file_id: Option<String>,
    #[serde(default, rename = "newFileTitle")]
    pub new_file_title: Option<String>,
    #[serde(default, rename = "newFileDescription")]
    pub new_file_description: Option<String>,
    #[serde(default, rename = "lineId")]
    pub line_id: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "neverSend")]
    pub never_send: Option<bool>,
    #[serde(default)]
    pub ph: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
}

const MAX_LINE_CHARS: usize = 500;
const MAX_TITLE_CHARS: usize = 80;
const MAX_DESCRIPTION_CHARS: usize = 240;

fn clip(s: &str, n: usize) -> String {
    s.trim().chars().filter(|c| !c.is_control()).take(n).collect()
}

/// Apply changes, in order. Returns what actually changed, for the "Memory updated"
/// sheet and its undo: `{op, fileId, fileTitle, lineId, text, before}`.
pub fn apply(l: &mut Local, ops: &[Op], now: u64) -> Vec<Value> {
    let mut done = Vec::new();
    for o in ops {
        // A file named by title is found by title (case aside) or made.
        let file_id = match (&o.file_id, &o.new_file_title) {
            (Some(id), _) if l.manifest.files.iter().any(|f| &f.id == id && !f.deleted) => Some(id.clone()),
            (_, Some(title)) if !title.trim().is_empty() => {
                let t = clip(title, MAX_TITLE_CHARS);
                match l.manifest.files.iter().find(|f| !f.deleted && f.title.to_lowercase() == t.to_lowercase()) {
                    Some(f) => Some(f.id.clone()),
                    None if o.op == "add" => {
                        let id = core::new_id("f_");
                        l.manifest.files.push(FileEntry {
                            id: id.clone(),
                            title: t,
                            description: clip(o.new_file_description.as_deref().unwrap_or(""), MAX_DESCRIPTION_CHARS),
                            version: 0,
                            never_send: false,
                            updated: now,
                            deleted: false,
                        });
                        l.files.insert(id.clone(), MemFile::new(&id));
                        l.touch(MANIFEST_ID, now);
                        Some(id)
                    }
                    None => None,
                }
            }
            _ => None,
        };
        match o.op.as_str() {
            "add" | "restore" => {
                let Some(fid) = file_id else { continue };
                let text = clip(o.text.as_deref().unwrap_or(""), MAX_LINE_CHARS);
                if text.is_empty() {
                    continue;
                }
                let file = l.files.entry(fid.clone()).or_insert_with(|| MemFile::new(&fid));
                let h = core::text_hash(&text);
                if file.lines.iter().any(|x| !x.deleted && core::text_hash(&x.text) == h) {
                    continue; // already known
                }
                let id = core::new_id("l_");
                file.lines.push(Line { id: id.clone(), text: text.clone(), never_send: o.never_send.unwrap_or(false), updated: now, deleted: false, h: String::new() });
                l.file_changed(&fid, now);
                done.push(json!({ "op": "add", "fileId": fid, "fileTitle": l.title_of(&fid), "lineId": id, "text": text }));
            }
            "update" => {
                let (Some(fid), Some(lid)) = (file_id, &o.line_id) else { continue };
                let text = clip(o.text.as_deref().unwrap_or(""), MAX_LINE_CHARS);
                let Some(line) = l.files.get_mut(&fid).and_then(|f| f.lines.iter_mut().find(|x| &x.id == lid && !x.deleted)) else { continue };
                if text.is_empty() || line.text == text {
                    continue;
                }
                let before = std::mem::replace(&mut line.text, text.clone());
                line.updated = now;
                l.file_changed(&fid, now);
                done.push(json!({ "op": "update", "fileId": fid, "fileTitle": l.title_of(&fid), "lineId": lid, "text": text, "before": before }));
            }
            "delete" => {
                let (Some(fid), Some(lid)) = (file_id, &o.line_id) else { continue };
                let Some(line) = l.files.get_mut(&fid).and_then(|f| f.lines.iter_mut().find(|x| &x.id == lid && !x.deleted)) else { continue };
                let before = line.text.clone();
                let never = line.never_send;
                core::tombstone(line, now);
                l.file_changed(&fid, now);
                done.push(json!({ "op": "delete", "fileId": fid, "fileTitle": l.title_of(&fid), "lineId": lid, "before": before, "neverSend": never }));
            }
            "line.flag" => {
                let (Some(fid), Some(lid)) = (file_id, &o.line_id) else { continue };
                let Some(line) = l.files.get_mut(&fid).and_then(|f| f.lines.iter_mut().find(|x| &x.id == lid && !x.deleted)) else { continue };
                line.never_send = o.never_send.unwrap_or(false);
                line.updated = now;
                l.file_changed(&fid, now);
                done.push(json!({ "op": "line.flag", "fileId": fid, "lineId": lid }));
            }
            "file.update" => {
                let Some(fid) = file_id else { continue };
                let Some(e) = l.entry_mut(&fid) else { continue };
                if let Some(t) = &o.title {
                    let t = clip(t, MAX_TITLE_CHARS);
                    if !t.is_empty() {
                        e.title = t;
                    }
                }
                if let Some(d) = &o.description {
                    e.description = clip(d, MAX_DESCRIPTION_CHARS);
                }
                if let Some(n) = o.never_send {
                    e.never_send = n;
                }
                e.updated = now;
                l.touch(MANIFEST_ID, now);
                done.push(json!({ "op": "file.update", "fileId": fid }));
            }
            "file.delete" => {
                let Some(fid) = file_id else { continue };
                let Some(e) = l.entry_mut(&fid) else { continue };
                e.deleted = true;
                e.updated = now;
                let title = e.title.clone();
                l.files.remove(&fid);
                l.dirty.remove(&fid);
                l.gone.insert(fid.clone());
                l.touch(MANIFEST_ID, now);
                done.push(json!({ "op": "file.delete", "fileId": fid, "fileTitle": title }));
            }
            "placeholder" => {
                let (Some(ph), Some(value)) = (&o.ph, &o.value) else { continue };
                if l.placeholders.map.iter().any(|p| &p.ph == ph) {
                    continue;
                }
                l.placeholders.map.push(Placeholder { ph: clip(ph, 40), value: clip(value, 200), kind: o.kind.clone().unwrap_or_default(), updated: now });
                l.touch(PLACEHOLDERS_ID, now);
            }
            _ => {}
        }
    }
    done
}

/// Merge what another device wrote (one blob, by file id) into the copy here. Returns how
/// many facts the merge changed here (added, changed or removed), for "updated from another
/// device (3 changes)".
pub fn merge_remote(l: &mut Local, file_id: &str, plain: &[u8]) -> Result<usize, String> {
    let live = |f: &MemFile| -> BTreeMap<String, String> { f.lines.iter().filter(|x| !x.deleted).map(|x| (x.id.clone(), x.text.clone())).collect() };
    match file_id {
        MANIFEST_ID => {
            let theirs: Manifest = serde_json::from_slice(plain).map_err(|e| format!("the palace index is unreadable: {e}"))?;
            let merged = core::merge_manifest(&l.manifest, &theirs);
            let changed = merged.files.iter().filter(|f| !l.manifest.files.contains(f)).count();
            // A file deleted elsewhere goes here too.
            for f in merged.files.iter().filter(|f| f.deleted) {
                l.files.remove(&f.id);
                l.dirty.remove(&f.id);
            }
            l.manifest = merged;
            Ok(changed)
        }
        PLACEHOLDERS_ID => {
            let theirs: Placeholders = serde_json::from_slice(plain).map_err(|e| format!("the placeholders are unreadable: {e}"))?;
            l.placeholders = core::merge_placeholders(&l.placeholders, &theirs);
            Ok(0)
        }
        id => {
            if l.manifest.files.iter().any(|f| f.id == id && f.deleted) {
                return Ok(0);
            }
            let theirs: MemFile = serde_json::from_slice(plain).map_err(|e| format!("a palace file is unreadable: {e}"))?;
            let mine = l.files.get(id).cloned().unwrap_or_else(|| MemFile::new(id));
            let merged = core::merge_file(&mine, &theirs);
            let (a, b) = (live(&mine), live(&merged));
            let changed = b.iter().filter(|(k, v)| a.get(*k) != Some(v)).count() + a.keys().filter(|k| !b.contains_key(*k)).count();
            l.files.insert(id.to_string(), merged);
            Ok(changed)
        }
    }
}

/// The plaintext of one blob, by file id.
pub fn plaintext(l: &Local, file_id: &str) -> Vec<u8> {
    match file_id {
        MANIFEST_ID => serde_json::to_vec(&l.manifest),
        PLACEHOLDERS_ID => serde_json::to_vec(&l.placeholders),
        id => serde_json::to_vec(&l.files.get(id).cloned().unwrap_or_else(|| MemFile::new(id))),
    }
    .unwrap_or_default()
}

/// Every file id whose blob there may be: the two fixed ones, and every file of the index.
pub fn known_ids(l: &Local) -> Vec<String> {
    let mut ids = vec![MANIFEST_ID.to_string(), PLACEHOLDERS_ID.to_string()];
    ids.extend(l.manifest.files.iter().filter(|f| !f.deleted).map(|f| f.id.clone()));
    ids
}

/// A failed attempt: wait longer each time, up to half an hour, and remember why.
pub fn failed(l: &mut Local, why: &str, offline: bool, now: u64) {
    l.attempts = l.attempts.saturating_add(1);
    let wait = (10_000u64 << l.attempts.min(12)).min(BACKOFF_MAX_MS);
    l.due_ms = now + wait + rand_between(0, wait / 4);
    l.last_error = Some(why.chars().take(200).collect());
    l.offline = offline;
}

pub fn succeeded(l: &mut Local, now: u64) {
    l.attempts = 0;
    l.last_error = None;
    l.offline = false;
    l.last_sync_ms = now;
    if l.dirty.is_empty() && l.gone.is_empty() {
        l.due_ms = 0;
    }
}

/// The indicator's state (`palace.sync.*` in the interface).
pub fn state(cfg: &Config, has_account: bool, syncing: bool, l: &Local) -> &'static str {
    if !has_account {
        return "locked";
    }
    if cfg.enabled != Some(true) {
        return "off";
    }
    if cfg.local_only {
        return "local_only";
    }
    if syncing {
        return "syncing";
    }
    if l.attempts >= ATTEMPTS_BEFORE_ERROR && !l.offline {
        return "error";
    }
    if l.offline && l.attempts > 0 {
        return "offline";
    }
    if !l.dirty.is_empty() || !l.gone.is_empty() {
        return "pending_upload";
    }
    "up_to_date"
}

// ---- the requests -------------------------------------------------------------------

/// A signed palace request body. The envelope it travels in is signed by a throwaway key
/// (`lib.rs`), never by the account.
pub fn body(keys: &PalaceKeys, op: &str, blob_id: &str, blob_ids: &[String], expected: u64, ct: &[u8]) -> (String, Value) {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    let nonce = core::new_id("n_") + &core::new_id("");
    let ts = now_ms();
    let joined = blob_ids.join(",");
    let (b, v) = match op {
        "get" => (joined.as_str(), 0),
        "put" => (blob_id, expected + 1),
        "delete" => (blob_id, expected),
        _ => ("", 0),
    };
    let hash = core::ct_hash(ct);
    let sig = keys.sign_request(&SignedFields { op, blob_id: b, version: v, ct_sha256: &hash, ts, nonce: &nonce });
    let mut body = json!({ "ns": keys.namespace(), "pub": keys.sign_pub_hex(), "ts": ts, "nonce": nonce, "sig": sig });
    match op {
        "get" => body["blobIds"] = json!(blob_ids),
        "put" => {
            body["blobId"] = json!(blob_id);
            body["expected"] = json!(expected);
            body["ct"] = json!(B64.encode(ct));
        }
        "delete" => {
            body["blobId"] = json!(blob_id);
            body["expected"] = json!(expected);
        }
        _ => {}
    }
    (format!("palace.{op}"), body)
}

#[cfg(test)]
mod tests {
    use super::*;
    const M: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon art";

    fn op(v: Value) -> Op {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn a_change_is_local_first_and_queued_with_a_delay() {
        let mut l = Local::default();
        let now = 1_000_000;
        let done = apply(&mut l, &[op(json!({ "op": "add", "newFileTitle": "Work", "newFileDescription": "Job, role", "text": "Works at [EMPLOYER_1]" }))], now);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0]["fileTitle"], "Work");
        let fid = done[0]["fileId"].as_str().unwrap().to_string();
        assert!(l.dirty.contains_key(&fid) && l.dirty.contains_key(MANIFEST_ID));
        assert!(l.due_ms >= now + UPLOAD_DELAY_MS.0 && l.due_ms <= now + UPLOAD_DELAY_MS.1);
        // The same fact again is not a change; a file named by its title is reused.
        assert!(apply(&mut l, &[op(json!({ "op": "add", "newFileTitle": "work", "text": "works at [EMPLOYER_1]." }))], now).is_empty());
        assert_eq!(l.manifest.files.len(), 1);
        // Undo of an add is a delete; undo of a delete comes back as a new line.
        let lid = done[0]["lineId"].as_str().unwrap();
        let del = apply(&mut l, &[op(json!({ "op": "delete", "fileId": fid, "lineId": lid }))], now + 1);
        assert_eq!(del[0]["before"], "Works at [EMPLOYER_1]");
        let back = apply(&mut l, &[op(json!({ "op": "restore", "fileId": fid, "text": "Works at [EMPLOYER_1]" }))], now + 2);
        assert_ne!(back[0]["lineId"], lid);
        assert_eq!(l.view()["files"][0]["lines"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn the_cache_opens_only_with_the_phrase_and_two_devices_converge() {
        let dir = std::env::temp_dir().join(format!("tokumai-palace-{}", core::new_id("")));
        let keys = PalaceKeys::from_mnemonic(M).unwrap();
        let mut phone = Local::default();
        let fid = apply(&mut phone, &[op(json!({ "op": "add", "newFileTitle": "Home", "text": "Lives in Berlin" }))], 1)[0]["fileId"].as_str().unwrap().to_string();
        save(&dir, &keys, &phone).unwrap();
        assert_eq!(load(&dir, &keys).manifest, phone.manifest);
        let stranger = PalaceKeys::from_mnemonic(&tokumai_core::account::create_account().mnemonic).unwrap();
        assert!(load(&dir, &stranger).manifest.files.is_empty(), "another phrase starts afresh");
        let _ = std::fs::remove_dir_all(&dir);
        // A second device with the same phrase gets the index and the file, merges them,
        // and adds its own fact; the phone merges that back.
        let mut laptop = Local::default();
        merge_remote(&mut laptop, MANIFEST_ID, &plaintext(&phone, MANIFEST_ID)).unwrap();
        assert_eq!(merge_remote(&mut laptop, &fid, &plaintext(&phone, &fid)).unwrap(), 1);
        apply(&mut laptop, &[op(json!({ "op": "add", "fileId": fid, "text": "Has a dog" }))], 5);
        apply(&mut phone, &[op(json!({ "op": "add", "fileId": fid, "text": "Cycles to work" }))], 6);
        assert_eq!(merge_remote(&mut phone, &fid, &plaintext(&laptop, &fid)).unwrap(), 1);
        merge_remote(&mut laptop, &fid, &plaintext(&phone, &fid)).unwrap();
        let texts = |l: &Local| {
            let mut t: Vec<String> = l.files[&fid].lines.iter().filter(|x| !x.deleted).map(|x| x.text.clone()).collect();
            t.sort();
            t
        };
        assert_eq!(texts(&phone), texts(&laptop));
        assert_eq!(texts(&phone).len(), 3);
    }

    #[test]
    fn the_indicator_says_what_is_true() {
        let on = Config { enabled: Some(true), local_only: false, wipe_pending: false };
        let mut l = Local::default();
        assert_eq!(state(&on, false, false, &l), "locked");
        assert_eq!(state(&Config::default(), true, false, &l), "off");
        assert_eq!(state(&on, true, true, &l), "syncing");
        assert_eq!(state(&on, true, false, &l), "up_to_date");
        apply(&mut l, &[op(json!({ "op": "add", "newFileTitle": "X", "text": "y" }))], 1);
        assert_eq!(state(&on, true, false, &l), "pending_upload");
        failed(&mut l, "no route", true, 2);
        assert_eq!(state(&on, true, false, &l), "offline");
        for _ in 0..ATTEMPTS_BEFORE_ERROR {
            failed(&mut l, "refused", false, 3);
        }
        assert_eq!(state(&on, true, false, &l), "error");
        assert!(l.due_ms <= 3 + BACKOFF_MAX_MS + BACKOFF_MAX_MS / 4);
    }
}
